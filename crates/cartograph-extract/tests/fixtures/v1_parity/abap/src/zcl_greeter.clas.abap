CLASS zcl_greeter DEFINITION PUBLIC CREATE PUBLIC.
  PUBLIC SECTION.
    INTERFACES zif_greeter.
    METHODS constructor IMPORTING iv_prefix TYPE string.
    METHODS greet IMPORTING iv_name TYPE string.
    CLASS-METHODS create RETURNING VALUE(ro_greeter) TYPE REF TO zcl_greeter.
  PRIVATE SECTION.
    DATA mv_prefix TYPE string.
    CONSTANTS gc_default TYPE string VALUE 'Hello'.
ENDCLASS.

CLASS zcl_greeter IMPLEMENTATION.
  METHOD constructor.
    mv_prefix = iv_prefix.
  ENDMETHOD.

  METHOD greet.
    DATA(lv_text) = |{ mv_prefix } { iv_name }|.
    WRITE lv_text.
    zcl_logger=>log( lv_text ).
  ENDMETHOD.

  METHOD create.
    ro_greeter = NEW zcl_greeter( gc_default ).
  ENDMETHOD.

  METHOD zif_greeter~greet.
    rv_text = iv_name.
  ENDMETHOD.

  METHOD zif_greeter~farewell.
    WRITE 'Bye'.
  ENDMETHOD.
ENDCLASS.
