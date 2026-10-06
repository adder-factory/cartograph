module Shapes

export Shape, Circle, Rect, area, describe

abstract type Shape end

primitive type Flag8 8 end

struct Circle <: Shape
    radius::Float64
end

mutable struct Rect <: Shape
    w::Float64
    h::Float64
end

struct Point
    x::Float64
    y::Float64
end

mutable struct Counter
    n::Int
end

const UNIT = 1.0

function bump!(c::Counter)
    c.n += 1
end

norm2(p::Point) = sqrt(p.x^2 + p.y^2)

area(c::Circle) = pi * c.radius^2

function area(r::Rect)::Float64
    return r.w * r.h
end

function describe(s::Shape)
    a = area(s)
    return label(s, a)
end

function label(s, a)
    string(typeof(s), ": ", round(a; digits=2))
end

macro checked(expr)
    return :(isnothing($expr) ? error("nothing") : $expr)
end

end
