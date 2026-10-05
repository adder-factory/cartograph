require_relative '../models/invoice'

class OrdersController < ApplicationController
  before_action :set_order

  def index
    @invoices = Billing::Invoice.build(10)
    repo = Repository.new
    repo.save(@invoices)
    Repository.new.save(@invoices)
    helper_function(1)
    current_user
  end

  def show
    invoice = Billing::Invoice.find(params[:id])
    invoice.finalize
  end

  def create
    Billing::Invoice.build(5).finalize
  end

  private

  def set_order
    @order = nil
  end
end
