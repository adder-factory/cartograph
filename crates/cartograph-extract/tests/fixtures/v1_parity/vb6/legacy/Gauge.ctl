VERSION 5.00
Begin VB.UserControl Gauge
   ClientHeight    =   600
End
Attribute VB_Name = "Gauge"
Option Explicit

Private mValue As Integer

Public Property Get Value() As Integer
    Value = mValue
End Property

Public Sub Redraw()
    Compute mValue
End Sub
