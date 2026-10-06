#include "shapes/shape.hpp"
#include <cmath>
#include <iostream>

namespace geo {

int Shape::s_count = 0;

Shape::Shape(std::string name) : name_(std::move(name)) {
    ++s_count;
}

int Shape::count() {
    return s_count;
}

void Shape::touch() {
    std::cout << name_ << std::endl;
}

Circle::Circle(double r) : Shape("circle"), radius_(r) {}

double Circle::area() const {
    return M_PI * radius_ * radius_;
}

void Circle::draw() const {
    std::cout << "circle " << area() << '\n';
}

Circle Circle::scaled(double k) const {
    return Circle(radius_ * detail::clamp(k, 0.0, 10.0));
}

namespace detail {
real_t clamp(real_t v, real_t lo, real_t hi) {
    return v < lo ? lo : (v > hi ? hi : v);
}
} // namespace detail

} // namespace geo
