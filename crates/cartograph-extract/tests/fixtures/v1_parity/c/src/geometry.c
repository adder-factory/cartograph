#include <stdio.h>
#include <stdlib.h>
#include "../include/geometry.h"
#include "util/strbuf.h"

#define API_EXPORT
#define AX_API

int g_polygon_count = 0;
static const char *k_name = "geom";

Point point_make(int x, int y) {
    Point p = { x, y };
    return p;
}

int point_dot(const Point *a, const Point *b) {
    return a->x * b->x + a->y * b->y;
}

Polygon *polygon_new(size_t count) {
    Polygon *poly = calloc(1, sizeof(Polygon));
    poly->points = calloc(count, sizeof(Point));
    poly->count = count;
    g_polygon_count++;
    return poly;
}

void polygon_free(Polygon *poly) {
    free(poly->points);
    free(poly);
}

u32 polygon_area(const Polygon *poly) {
    u32 total = 0;
    for (size_t i = 0; i < poly->count; i++) {
        total += (u32)SQUARE(poly->points[i].x);
    }
    return total > MAX_POINTS ? MAX_POINTS : total;
}

API_EXPORT int api_describe(const Polygon *poly) {
    strbuf *sb = strbuf_new();
    strbuf_append(sb, k_name);
    printf("%s %zu\n", sb->data, poly->count);
    return (int)sb->len;
}

AX_API u32 AX_Init(void) {
    return polygon_area(NULL);
}

typedef int (*visit_fn)(const Point *p);

static int visit_all(const Polygon *poly, visit_fn fn) {
    int n = 0;
    for (size_t i = 0; i < poly->count; i++) {
        n += fn(&poly->points[i]);
    }
    return n;
}
