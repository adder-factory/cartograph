Attribute VB_Name = "Module1"
Option Explicit

Private Declare PtrSafe Function GetTickCount Lib "kernel32" () As Long
Public Declare Sub Sleep Lib "kernel32" (ByVal ms As Long)

Public Const APP_NAME As String = "Billing"
Private Const MAX_ITEMS = 10
Public gCounter As Long
Dim mBuffer As String

Public Type InvoiceLine
    Sku As String
    Qty As Integer
End Type

Public Enum Status
    stNew = 0
    stPaid
    stVoid
End Enum

Public Sub Main()
    Dim i As Integer
    Call DoWork(1)
    Helper i
    MsgBox "hi"
    gCounter = Compute(2)
    Sleep 10
End Sub

Public Function Compute(ByVal x As Integer) As Integer
    Compute = x * GetTickCount()
End Function

Private Sub DoWork(ByVal n As Integer)
    Helper n
End Sub

Friend Sub Helper(ByVal n As Integer)
    Debug.Print n
End Sub
