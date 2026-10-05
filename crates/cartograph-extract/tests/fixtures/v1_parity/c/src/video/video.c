#include "util/strbuf.h"
#include "geometry.h"

typedef int AX_S32;
#define AX_VOID void

AX_VIN_GLB_API AX_S32 AX_VIN_Init(AX_VOID) {
    return 0;
}

struct frame {
    int width;
    int height;
};

int frame_pixels(struct frame *f) {
    strbuf *log = strbuf_new();
    strbuf_append(log, "frame");
    return f->width * f->height + AX_VIN_Init();
}
