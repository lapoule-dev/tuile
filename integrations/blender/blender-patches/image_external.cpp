/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: Apache-2.0 */

#include "scene/image_external.h"

#include "util/image_metadata.h"
#include "util/log.h"

#include <OpenImageIO/filesystem.h>

#include <cstdlib>
#include <mutex>

#ifndef _WIN32
#  include <dlfcn.h>
#endif

CCL_NAMESPACE_BEGIN

namespace {

using OpenFn = int (*)(const char *, const unsigned char **, size_t *, uint64_t *);
using CloseFn = void (*)(uint64_t);

struct Source {
  OpenFn open = nullptr;
  CloseFn close = nullptr;
};

/* Resolved once, on first use.
 *
 * Once, because `dlopen` on every texture would be a syscall per image for an
 * answer that cannot change, and because a missing library must be reported
 * once rather than per tile — a render of forty thousand tiles would otherwise
 * write forty thousand identical warnings and hide everything else. */
const Source &the_source()
{
  static Source source;
  static std::once_flag once;
  std::call_once(once, []() {
#ifndef _WIN32
    const char *path = getenv("CYCLES_EXTERNAL_IMAGE_LIB");
    if (path == nullptr || path[0] == '\0') {
      return;
    }
    /* Kept open deliberately: the handles this library hands out stay valid
     * until they are closed, and closing the library would unmap them. */
    void *lib = dlopen(path, RTLD_LAZY | RTLD_LOCAL);
    if (lib == nullptr) {
      LOG_WARNING << "CYCLES_EXTERNAL_IMAGE_LIB=" << path << " did not load: " << dlerror();
      return;
    }
    source.open = (OpenFn)dlsym(lib, "cycles_external_image_open");
    source.close = (CloseFn)dlsym(lib, "cycles_external_image_close");
    if (source.open == nullptr || source.close == nullptr) {
      LOG_WARNING << path << " has no cycles_external_image_open/close; images will be read "
                  << "from the filesystem";
      source.open = nullptr;
      source.close = nullptr;
      return;
    }
    LOG_INFO << "external image source: " << path;
#endif
  });
  return source;
}

/* Encoded bytes borrowed from the embedder, decoded by OIIO like a file. */
class ExternalImageLoader : public ImageLoader {
 public:
  ExternalImageLoader(string name, const unsigned char *data, const size_t size, uint64_t handle)
      : name_(std::move(name)), data_(data), size_(size), handle_(handle)
  {
  }

  ~ExternalImageLoader() override
  {
    release();
  }

  bool load_metadata(ImageMetaData &metadata,
                     const ImageLoaderParams & /*params*/,
                     Progress & /*progress*/) override
  {
    OIIO::Filesystem::IOMemReader reader(data_, size_);
    return metadata.oiio_load_metadata(name_, nullptr, &reader);
  }

  bool load_pixels(const ImageMetaData &metadata, void *pixels) override
  {
    OIIO::Filesystem::IOMemReader reader(data_, size_);
    if (!metadata.oiio_load_pixels(name_, pixels, true, &reader)) {
      return false;
    }
    metadata.conform_pixels(pixels);
    return true;
  }

  string name() const override
  {
    return name_;
  }

  /* Two loaders are the same image when they carry the same name — the same
   * rule `OIIOImageLoader` uses for two filepaths, and the reason the name has
   * to carry everything that decides the pixels. */
  bool equals(const ImageLoader &other) const override
  {
    const ExternalImageLoader *o = dynamic_cast<const ExternalImageLoader *>(&other);
    return o != nullptr && o->name_ == name_;
  }

  /* Cycles calls this once the pixels are on the device. Giving the borrow back
   * here rather than in the destructor is what lets the embedder free an image
   * while the loader itself is still referenced by the image cache. */
  void cleanup() override
  {
    release();
  }

 private:
  void release()
  {
    if (handle_ != 0) {
      the_source().close(handle_);
      handle_ = 0;
      data_ = nullptr;
      size_ = 0;
    }
  }

  string name_;
  const unsigned char *data_ = nullptr;
  size_t size_ = 0;
  uint64_t handle_ = 0;
};

}  // namespace

unique_ptr<ImageLoader> external_image_loader(const string &name)
{
  const Source &source = the_source();
  if (source.open == nullptr) {
    return nullptr;
  }
  const unsigned char *data = nullptr;
  size_t size = 0;
  uint64_t handle = 0;
  if (source.open(name.c_str(), &data, &size, &handle) == 0 || data == nullptr || size == 0) {
    return nullptr;
  }
  return make_unique<ExternalImageLoader>(name, data, size, handle);
}

CCL_NAMESPACE_END
