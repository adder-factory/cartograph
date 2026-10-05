Imports System
Imports Billing

Module Program
    Private Const Greeting As String = "hi"

    Sub Main()
        Dim p As Person = Person.Create()
        Dim total As Integer = Add(1, 2)
        Console.WriteLine(p.Greet(Greeting))
        Report(total)
        PROCESS(total)
    End Sub

    Function Add(a As Integer, b As Integer) As Integer
        Return a + b
    End Function

    Private Sub Report(value As Integer)
        Console.WriteLine(value)
    End Sub

    Sub Process(value As Integer)
        Report(value)
    End Sub
End Module

Class C
    Public x As Integer

    Sub F()
        Helper()
    End Sub

    Sub Helper()
    End Sub
End Class
