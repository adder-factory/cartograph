package bus_pkg;
  typedef enum logic [1:0] { IDLE, READ, WRITE } state_t;

  typedef struct packed {
    logic [7:0] addr;
    logic [31:0] data;
  } packet_t;

  function automatic int parity(input logic [31:0] data);
    return ^data;
  endfunction

  class Transaction;
    rand bit [7:0] addr;
    function new();
      addr = 0;
    endfunction
    function void display();
      $display("addr=%0d", addr);
    endfunction
    task run();
      display();
    endtask
  endclass
endpackage

interface bus_if (input logic clk);
  logic valid;
  logic ready;
  modport master (output valid, input ready);
endinterface
