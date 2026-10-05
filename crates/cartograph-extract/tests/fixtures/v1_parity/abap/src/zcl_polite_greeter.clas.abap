CLASS zcl_polite_greeter DEFINITION PUBLIC INHERITING FROM zcl_greeter.
  PUBLIC SECTION.
    METHODS greet REDEFINITION.
ENDCLASS.

CLASS zcl_polite_greeter IMPLEMENTATION.
  METHOD greet.
    super->greet( iv_name ).
    zcl_logger=>log( 'polite' ).
  ENDMETHOD.
ENDCLASS.
