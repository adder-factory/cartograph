// Simple up-counter with an enable.
module counter #(parameter WIDTH = 4) (
  input  logic             clk,
  input  logic             rst_n,
  input  logic             en,
  output logic [WIDTH-1:0] value
);
  function automatic logic [WIDTH-1:0] next_value(input logic [WIDTH-1:0] cur);
    return cur + 1;
  endfunction

  task automatic reset_value();
    value = '0;
  endtask

  always_ff @(posedge clk or negedge rst_n) begin
    if (!rst_n) value <= '0;
    else if (en) value <= next_value(value);
  end
endmodule
