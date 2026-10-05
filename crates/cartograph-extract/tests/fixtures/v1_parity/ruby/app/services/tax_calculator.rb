MAX_RATE = 0.5
default_region = "eu"

class TaxCalculator
  def self.compute(amount, rate)
    amount * clamp(rate)
  end

  def self.clamp(rate)
    [rate, MAX_RATE].min
  end
end

class Repository
  def save(item)
    item
  end
end

def helper_function(value)
  value.to_s
end
