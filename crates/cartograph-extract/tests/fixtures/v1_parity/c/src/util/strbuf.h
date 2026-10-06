#ifndef STRBUF_H
#define STRBUF_H

#include <stddef.h>

typedef struct strbuf {
    char *data;
    size_t len;
} strbuf;

strbuf *strbuf_new(void);
void strbuf_append(strbuf *sb, const char *text);

#endif
