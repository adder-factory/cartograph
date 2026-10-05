`include "defs.vh"
import bus_pkg::*;

module top_tb;
  logic clk;
  logic rst_n;
  logic en;
  logic [3:0] value;
  logic [8:0] sum;

  counter #(.WIDTH(4)) u_counter (.clk(clk), .rst_n(rst_n), .en(en), .value(value));
  adder u_adder (.a(8'd1), .b(8'd2), .sum(sum));

  task automatic tick();
    clk = 1; #1; clk = 0; #1;
  endtask

  initial begin
    Transaction t = new();
    t.run();
    tick();
    $finish;
  end
endmodule
