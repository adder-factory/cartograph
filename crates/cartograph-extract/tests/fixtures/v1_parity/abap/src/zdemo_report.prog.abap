REPORT zdemo_report.

CLASS lcl_app DEFINITION.
  PUBLIC SECTION.
    METHODS run.
ENDCLASS.

CLASS lcl_app IMPLEMENTATION.
  METHOD run.
    DATA(lo_greeter) = zcl_greeter=>create( ).
    lo_greeter->greet( 'World' ).
    PERFORM say_done.
  ENDMETHOD.
ENDCLASS.

FORM say_done.
  WRITE 'Done'.
ENDFORM.

START-OF-SELECTION.
  NEW lcl_app( )->run( ).
