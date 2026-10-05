#ifndef GEOMETRY_H
#define GEOMETRY_H

#include <stddef.h>

#define GEOM_VERSION "1.2"
#define MAX_POINTS 128
#define HAVE_FAST_MATH
#define SQUARE(x) ((x) * (x))

typedef unsigned int u32;

typedef struct {
    int x;
    int y;
} Point;

struct Polygon {
    Point *points;
    size_t count;
    struct Polygon *next;
};

typedef struct Polygon Polygon;

enum Shape {
    SHAPE_NONE,
    SHAPE_POINT = 1,
    SHAPE_POLY
};

typedef enum {
    COLOR_RED,
    COLOR_GREEN
} Color;

union Number {
    int i;
    float f;
};

extern int g_polygon_count;

Point point_make(int x, int y);
int point_dot(const Point *a, const Point *b);
Polygon *polygon_new(size_t count);
void polygon_free(Polygon *poly);
u32 polygon_area(const Polygon *poly);

#endif
