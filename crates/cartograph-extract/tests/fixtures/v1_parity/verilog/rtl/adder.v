// Classic Verilog-2001 adder.
module adder (a, b, sum);
  input  [7:0] a;
  input  [7:0] b;
  output [8:0] sum;

  function [8:0] add8;
    input [7:0] x;
    input [7:0] y;
    begin
      add8 = x + y;
    end
  endfunction

  assign sum = add8(a, b);
endmodule
