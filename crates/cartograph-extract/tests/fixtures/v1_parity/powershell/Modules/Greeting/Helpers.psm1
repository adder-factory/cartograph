using namespace System.Text

$DefaultGreeting = "Hello"
$script:Counter = 0

function Format-Greeting {
    param(
        [string] $Name,
        [string] $Greeting = $DefaultGreeting
    )
    $builder = [StringBuilder]::new()
    [void] $builder.Append("$Greeting, $Name")
    Write-Verbose "formatted"
    return $builder.ToString()
}

function Get-Timestamp {
    Get-Date -Format "yyyy-MM-dd" | Out-String
}

Export-ModuleMember -Function Format-Greeting, Get-Timestamp
