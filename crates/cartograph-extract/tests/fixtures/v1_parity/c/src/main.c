#include <stdio.h>
#include "../include/geometry.h"

static int print_point(const Point *p) {
    return printf("(%d,%d)\n", p->x, p->y);
}

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    Polygon *poly = polygon_new(4);
    Point a = point_make(1, 2);
    Point b = point_make(3, 4);
    int dot = point_dot(&a, &b);
    enum Shape shape = SHAPE_POLY;
    Color c = COLOR_RED;
    print_point(&a);
    printf("%d %u %d %d\n", dot, polygon_area(poly), shape, c);
    polygon_free(poly);
    return 0;
}
