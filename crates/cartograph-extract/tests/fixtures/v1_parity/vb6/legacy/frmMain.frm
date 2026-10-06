VERSION 5.00
Begin VB.Form frmMain
   Caption         =   "Billing"
   ClientHeight    =   3000
   Begin VB.CommandButton Command1
      Caption         =   "Go"
   End
End
Attribute VB_Name = "frmMain"
Attribute VB_Creatable = False
Option Explicit

Private WithEvents mCustomer As Customer

Private Sub Form_Load()
    Set mCustomer = New Customer
    mCustomer.Load
End Sub

Private Sub Command1_Click()
    Main
    Call RefreshView
End Sub

Private Sub RefreshView()
    Me.Caption = APP_NAME
End Sub
