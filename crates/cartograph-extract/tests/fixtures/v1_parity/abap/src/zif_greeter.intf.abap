INTERFACE zif_greeter PUBLIC.
  METHODS greet IMPORTING iv_name TYPE string RETURNING VALUE(rv_text) TYPE string.
  METHODS farewell.
ENDINTERFACE.
