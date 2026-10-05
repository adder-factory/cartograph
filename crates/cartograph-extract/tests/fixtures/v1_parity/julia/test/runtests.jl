using Test
include("../src/Report.jl")

@testset "report" begin
    @test Report.build() !== nothing
    run_checks()
end

function run_checks()
    summarize([])
end
