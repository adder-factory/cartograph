#include <stdlib.h>
#include <string.h>
#include "strbuf.h"

static size_t grow(size_t len) {
    return len * 2 + 1;
}

strbuf *strbuf_new(void) {
    strbuf *sb = malloc(sizeof(strbuf));
    sb->data = NULL;
    sb->len = 0;
    return sb;
}

void strbuf_append(strbuf *sb, const char *text) {
    size_t n = strlen(text);
    sb->data = realloc(sb->data, grow(sb->len + n));
    memcpy(sb->data + sb->len, text, n);
    sb->len += n;
}
