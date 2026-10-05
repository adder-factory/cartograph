#pragma once

#include <string>
#include <vector>
#include <memory>

#define SHAPES_API
#define MAX_SHAPES 32

namespace geo {

struct Vec2 {
    double x;
    double y;
};

enum class Kind {
    Circle,
    Square
};

enum Legacy { LEGACY_A, LEGACY_B };

using Points = std::vector<Vec2>;
typedef double real_t;

class Drawable {
public:
    virtual ~Drawable() = default;
    virtual void draw() const = 0;
};

class Shape : public Drawable {
public:
    explicit Shape(std::string name);
    virtual double area() const = 0;
    const std::string &name() const { return name_; }
    static int count();

protected:
    void touch();

private:
    std::string name_;
    static int s_count;
};

class Circle final : public Shape, private Vec2 {
public:
    Circle(double r);
    double area() const override;
    void draw() const override;
    Circle scaled(double k) const;

private:
    double radius_;
};

template <typename T>
class Registry {
public:
    void add(std::unique_ptr<T> item) { items_.push_back(std::move(item)); }
    std::size_t size() const { return items_.size(); }

private:
    std::vector<std::unique_ptr<T>> items_;
};

namespace detail {
real_t clamp(real_t v, real_t lo, real_t hi);
}

} // namespace geo
