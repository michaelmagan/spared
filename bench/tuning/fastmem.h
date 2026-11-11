/* Force-included into sqlite3.c (-include): a word-at-a-time memcmp in place of the byte loop in wasi-libc. */
#ifndef SPARED_FASTMEM_H
#define SPARED_FASTMEM_H
#include <stddef.h>
#include <stdint.h>
#include <string.h>
static inline int spared_memcmp(const void *a, const void *b, size_t n) {
  const unsigned char *p = (const unsigned char *)a, *q = (const unsigned char *)b;
  while (n >= 8) {
    uint64_t x, y;
    __builtin_memcpy(&x, p, 8);
    __builtin_memcpy(&y, q, 8);
    if (x != y) break;
    p += 8; q += 8; n -= 8;
  }
  while (n) {
    if (*p != *q) return (int)*p - (int)*q;
    p++; q++; n--;
  }
  return 0;
}
#define memcmp spared_memcmp
#endif
