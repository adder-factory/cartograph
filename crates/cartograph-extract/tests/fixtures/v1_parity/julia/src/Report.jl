module Report

using LinearAlgebra
import Statistics: mean
include("Shapes.jl")
using .Shapes

function summarize(shapes)
    areas = [Shapes.area(s) for s in shapes]
    total = sum(areas)
    avg = mean(areas)
    @info "summary" total avg
    print_report(total, avg)
end

function print_report(total, avg)
    println("total=", total, " avg=", avg)
end

function build()
    c = Shapes.Circle(2.0)
    r = Shapes.Rect(1.0, 3.0)
    summarize([c, r])
    describe(c)
end

end
