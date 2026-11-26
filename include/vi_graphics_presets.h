#pragma once
#include <stddef.h>
#include <stdint.h>

/* Windows x64, before title code runs. Pin the DLL until process exit.
 * original_hash is exactly 64 lowercase ASCII BLAKE3 hex bytes, not a C string.
 * Both paths are absolute, NUL-terminated UTF-16. The log parent must exist.
 * Returns 0 on installation success; a nonzero status rejects installation.
 */
#ifdef __cplusplus
extern "C" {
#endif
uint32_t __stdcall gp_install_v1(
    uintptr_t image_base,
    uint32_t image_size,
    const uint8_t *original_hash,
    size_t hash_len,
    const uint16_t *preset_path,
    const uint16_t *log_path);
#ifdef __cplusplus
}
#endif
