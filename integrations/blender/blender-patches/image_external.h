/* SPDX-FileCopyrightText: 2026 lapoule.dev
 *
 * SPDX-License-Identifier: Apache-2.0 */

/* An image whose bytes come from the embedder, not from a path.
 *
 * Cycles resolves every texture to a file: `ImageManager::add_image` builds an
 * `OIIOImageLoader` around a filename, and `ImageMetaData::oiio_load_metadata`
 * refuses anything `OIIO::Filesystem::exists()` does not answer for. An
 * application that already holds the encoded bytes — a renderer fed from an
 * archive, a database, a network cache — therefore has to write each image to a
 * temporary file first, and on a host whose `/tmp` is a tmpfs that is a second
 * copy of every texture in RAM.
 *
 * This adds one seam. A shared library named by `CYCLES_EXTERNAL_IMAGE_LIB` may
 * answer for names Cycles would otherwise open as files:
 *
 *     int  cycles_external_image_open(const char *name,
 *                                     const unsigned char **data,
 *                                     size_t *size,
 *                                     uint64_t *handle);   // 1 = mine, 0 = not
 *     void cycles_external_image_close(uint64_t handle);
 *
 * The bytes stay **encoded** — a PNG, a JPEG — and are decoded by OIIO exactly
 * as a file would be, through an `IOProxy`. Nothing here decodes anything, and
 * the format is still chosen by the name's extension, so an external image is
 * the same image it would have been on disk.
 *
 * A library that answers 0, a library that cannot be loaded, and an unset
 * variable are all the same thing: Cycles opens the file, as before. */

#ifndef __IMAGE_EXTERNAL_H__
#define __IMAGE_EXTERNAL_H__

#include "scene/image_loader.h"

#include "util/string.h"
#include "util/unique_ptr.h"

CCL_NAMESPACE_BEGIN

/* A loader for `name`, or null when no external source claims it. */
unique_ptr<ImageLoader> external_image_loader(const string &name);

CCL_NAMESPACE_END

#endif /* __IMAGE_EXTERNAL_H__ */
