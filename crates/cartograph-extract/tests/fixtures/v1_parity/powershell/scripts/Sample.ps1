using module ../Modules/Greeting/Helpers.psm1
using assembly System.Net.Http
using namespace System.Collections.Generic

enum Mood {
    Happy = 1
    Grumpy
}

class Greeter {
    [string] $Name
    [Mood] $Mood = [Mood]::Happy
    static [int] $Instances = 0

    Greeter([string] $name) {
        $this.Name = $name
        [Greeter]::Instances++
    }

    [string] Greet([string] $target) {
        return Format-Greeting -Name $target
    }

    [void] Log() {
        Write-Host $this.Greet($this.Name)
        $this.Stamp()
    }

    hidden [void] Stamp() {
        Get-Timestamp | Out-Null
    }
}

class LoudGreeter : Greeter {
    LoudGreeter([string] $name) : base($name) {
    }

    [string] Greet([string] $target) {
        return (Format-Greeting -Name $target).ToUpper()
    }
}

$Config = @{ Retries = 3 }
$Names = @('Ada', 'Grace')

function Invoke-Main {
    [CmdletBinding()]
    param([string[]] $People)
    foreach ($p in $People) {
        $g = [Greeter]::new($p)
        $g.Log()
    }
    Invoke-Cleanup
}

function Invoke-Cleanup {
    Remove-Variable -Name Config -ErrorAction SilentlyContinue
}

Invoke-Main -People $Names
Format-Greeting -Name Ada
