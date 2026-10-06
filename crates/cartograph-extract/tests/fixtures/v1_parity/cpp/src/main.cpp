#include <iostream>
#include <memory>
#include "shapes/shape.hpp"
#include "client.hpp"

using namespace geo;

struct Widget {
    int doWork() { return 0; }
    int field;
};

static int use(Widget *w) {
    return w->doWork() + w->field;
}

void run() {
    Client::create().commit();
}

int main() {
    auto c = std::make_unique<Circle>(2.0);
    Circle *raw = new Circle(1.0);
    Registry<Shape> reg;
    reg.add(std::move(c));
    raw->draw();
    double a = raw->scaled(2.0).area();
    int n = Shape::count();
    Widget w{};
    auto twice = [](int v) { return v * 2; };
    std::cout << a << n << use(&w) << twice(MAX_SHAPES) << reg.size() << std::endl;
    run();
    delete raw;
    return deal_command("eth0", 1);
}
