require 'json'
require 'active_support/core_ext/string'
require_relative 'application_record'
require_relative '../services/tax_calculator'

module Billing
  module Auditable
    def audit
      log_event
    end

    def log_event
      true
    end
  end

  class Invoice < ApplicationRecord
    include Auditable

    RATE = 0.2
    STATES = %w[draft paid].freeze

    attr_reader :total, :tax
    attr_writer :note
    attr_accessor :status
    class_attribute :default_currency

    def initialize(total)
      @total = total
      @tax = TaxCalculator.compute(total, RATE)
    end

    def self.build(total)
      new(total)
    end

    def finalize
      reset
      audit
      calculate_total
      save
    end

    def to_json
      JSON.generate(total: total)
    end

    private

    def reset
      @status = "draft"
    end

    def calculate_total
      @total + @tax
    end

    protected

    def internal_note
      "hidden"
    end
  end
end
