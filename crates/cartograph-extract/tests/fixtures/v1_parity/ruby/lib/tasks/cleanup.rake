namespace :cleanup do
  task :run do
    TaxCalculator.compute(1, 0.1)
  end
end
