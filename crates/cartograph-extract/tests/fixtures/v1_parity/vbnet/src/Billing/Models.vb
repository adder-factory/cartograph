Imports System
Imports System.Collections.Generic

Namespace Billing
    Public Interface IGreeter
        Function Greet(name As String) As String
    End Interface

    Public Structure Pt
        Public X As Integer
        Public Y As Integer
    End Structure

    Public Enum Color
        Red
        Green = 2
        Blue
    End Enum

    Public Class Person
        Implements IGreeter

        Private _name As String
        Friend Shared Count As Integer = 0
        Public Property Nickname As String

        Public Sub New(name As String)
            _name = name
            Count += 1
        End Sub

        Public Function Greet(value As String) As String Implements IGreeter.Greet
            Return Helper(value) & _name
        End Function

        Private Function Helper(value As String) As String
            Return value.Trim()
        End Function

        Public Shared Function Create() As Person
            Return New Person("anon")
        End Function
    End Class
End Namespace
