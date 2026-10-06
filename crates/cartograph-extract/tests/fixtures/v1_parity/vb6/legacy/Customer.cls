VERSION 1.0 CLASS
BEGIN
  MultiUse = -1  'True
END
Attribute VB_Name = "Customer"
Attribute VB_GlobalNameSpace = False
Option Explicit

Private customerName As String
Public Balance As Currency

Public Property Get Name() As String
    Name = customerName
End Property

Public Property Let Name(ByVal value As String)
    customerName = FormatName(value)
End Property

Public Sub Load()
    FormatName customerName
    Module1.Helper 3
End Sub

Public Function FormatName(ByVal value As String) As String
    FormatName = Trim$(value)
End Function
