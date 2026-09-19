// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#include "tiles.h"

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <unordered_map>
#include <vector>

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

/// Un fichier, pas trois mille.
///
/// Cette table tenait chaque drape dans un `vector<uint8_t>` de son côté, et le
/// lecteur de pack en écrivait *en plus* un PNG par tuile dans `/tmp`. Sur
/// Cloud Run `/tmp` est un tmpfs : les deux étaient de la RAM, dans un job qui
/// en a trente-deux gigaoctets, pour un pack d'imagerie qui en pèse deux.
///
/// Les octets sont désormais **déversés une fois** dans un seul fichier, et
/// servis par une fenêtre de `mmap` dessus. Un `mmap` d'un tmpfs ne recopie
/// rien : ce sont les pages du fichier, pas un double. Il reste donc un
/// exemplaire, un inode, et aucune écriture par texture.
///
/// Le déversoir est par processus. Quatre rendus sur une tâche lisent des
/// plages de frames disjointes, donc leurs drapes le sont presque entièrement ;
/// un fichier partagé demanderait un verrou entre processus et un index
/// partagé pour économiser une intersection quasi vide.
namespace {

struct Entry
{
    size_t offset = 0;
    size_t size = 0;
};

struct Store
{
    std::mutex mutex;
    /// URI → où les octets ont été déversés.
    std::unordered_map<std::string, Entry> index;
    /// Handle → l'entrée qu'on lit encore. Séparé de `index` pour la même
    /// raison qu'avant : republier une URI ne doit pas retirer les octets sous
    /// une lecture déjà en vol.
    std::unordered_map<uint64_t, Entry> borrowed;
    uint64_t nextHandle = 1;

    /// Le déversoir, ouvert à la première écriture.
    int fd = -1;
    std::string path;
    size_t written = 0;
    /// La projection courante du déversoir. Refaite quand il grandit —
    /// `mremap` n'est pas portable et ce n'est pas un chemin chaud : une
    /// reprojection par écriture, pas par lecture.
    uint8_t *mapped = nullptr;
    size_t mappedSize = 0;
};

Store &_TheStore()
{
    static Store store;
    return store;
}

/// Ouvre le déversoir et le programme pour disparaître.
///
/// Délié tout de suite après ouverture : le fichier n'a plus de nom, donc
/// personne ne peut l'ouvrir par accident, il ne survit pas au processus même
/// tué net, et il n'y a rien à nettoyer sur une tâche interrompue. C'est ce que
/// `tmpfile(3)` fait, écrit à la main parce qu'il faut le descripteur.
bool _OpenSpill(Store &store)
{
    if (store.fd >= 0) {
        return true;
    }
    const char *dir = getenv("TUILE_TEXTURE_SPILL_DIR");
    if (dir == nullptr || dir[0] == '\0') {
        dir = getenv("TMPDIR");
    }
    if (dir == nullptr || dir[0] == '\0') {
        dir = "/tmp";
    }
    std::string templ = std::string(dir) + "/tuile-textures-XXXXXX";
    std::vector<char> buf(templ.begin(), templ.end());
    buf.push_back('\0');
    const int fd = mkstemp(buf.data());
    if (fd < 0) {
        fprintf(stderr,
                "tuile: no texture spill in %s: %s — textures will not resolve\n",
                dir, strerror(errno));
        return false;
    }
    unlink(buf.data());
    store.fd = fd;
    store.path.assign(buf.data());
    return true;
}

/// (Re)projette le déversoir entier. À appeler le verrou tenu.
bool _Remap(Store &store)
{
    if (store.written == 0) {
        return true;
    }
    if (store.mapped != nullptr && store.mappedSize == store.written) {
        return true;
    }
    if (store.mapped != nullptr) {
        munmap(store.mapped, store.mappedSize);
        store.mapped = nullptr;
        store.mappedSize = 0;
    }
    void *at = mmap(nullptr, store.written, PROT_READ, MAP_SHARED, store.fd, 0);
    if (at == MAP_FAILED) {
        fprintf(stderr, "tuile: mapping the texture spill failed: %s\n", strerror(errno));
        return false;
    }
    store.mapped = static_cast<uint8_t *>(at);
    store.mappedSize = store.written;
    return true;
}

}  // namespace

void
TuileSpikeTiles::Put(const std::string &uri, std::vector<uint8_t> bytes)
{
    if (bytes.empty()) {
        return;
    }
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);
    if (!_OpenSpill(store)) {
        return;
    }
    // Écrit à la fin, en une fois. `pwrite` plutôt que `write` parce que
    // plusieurs threads déversent et que la position du descripteur est
    // partagée entre eux — décider soi-même de l'offset est la seule façon de
    // ne pas dépendre d'un curseur que le voisin déplace.
    const size_t offset = store.written;
    size_t done = 0;
    while (done < bytes.size()) {
        const ssize_t n = pwrite(store.fd, bytes.data() + done, bytes.size() - done,
                                 static_cast<off_t>(offset + done));
        if (n <= 0) {
            if (errno == EINTR) {
                continue;
            }
            fprintf(stderr, "tuile: spilling %s failed: %s\n", uri.c_str(), strerror(errno));
            return;
        }
        done += static_cast<size_t>(n);
    }
    store.written = offset + bytes.size();
    if (!_Remap(store)) {
        return;
    }
    store.index[uri] = Entry{offset, bytes.size()};
}

bool
TuileSpikeTiles::Has(const std::string &uri)
{
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);
    return store.index.find(uri) != store.index.end();
}

TuileSpikeTiles::Bytes
TuileSpikeTiles::Get(const std::string &uri)
{
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);

    auto it = store.index.find(uri);
    if (it == store.index.end() || store.mapped == nullptr) {
        return Bytes();
    }
    const Entry entry = it->second;
    if (entry.offset + entry.size > store.mappedSize) {
        return Bytes();
    }

    const uint64_t handle = store.nextHandle++;
    store.borrowed[handle] = entry;

    Bytes bytes;
    bytes.data = store.mapped + entry.offset;
    bytes.size = entry.size;
    bytes.handle = handle;
    return bytes;
}

void
TuileSpikeTiles::Release(uint64_t handle)
{
    if (handle == 0) {
        return;
    }
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);
    store.borrowed.erase(handle);
}

void
TuileSpikeTiles::Clear()
{
    Store &store = _TheStore();
    std::lock_guard<std::mutex> lock(store.mutex);
    // L'index seulement. Le déversoir garde ses octets : les relire coûte une
    // page déjà résidente, alors que le tronquer invaliderait les emprunts en
    // vol et rendrait les offsets déjà distribués faux.
    store.index.clear();
}

// ---------------------------------------------------------------------------
// Ce que le fork de Cycles appelle.
//
// `intern/cycles/scene/image_external.cpp` charge cette bibliothèque par
// `CYCLES_EXTERNAL_IMAGE_LIB` et résout ces deux symboles. Il ne connaît ni
// notre table ni notre déversoir : il reçoit une fenêtre d'octets encodés et
// laisse OIIO décoder, par l'extension du nom, exactement comme pour un
// fichier. Un nom qui n'est pas à nous rend 0, et Cycles ouvre le fichier.
// ---------------------------------------------------------------------------

extern "C" int
cycles_external_image_open(const char *name,
                           const unsigned char **data,
                           size_t *size,
                           uint64_t *handle)
{
    if (name == nullptr || data == nullptr || size == nullptr || handle == nullptr) {
        return 0;
    }
    const TuileSpikeTiles::Bytes bytes = TuileSpikeTiles::Get(std::string(name));
    if (bytes.data == nullptr || bytes.size == 0) {
        return 0;
    }
    *data = bytes.data;
    *size = bytes.size;
    *handle = bytes.handle;
    return 1;
}

extern "C" void
cycles_external_image_close(uint64_t handle)
{
    TuileSpikeTiles::Release(handle);
}
