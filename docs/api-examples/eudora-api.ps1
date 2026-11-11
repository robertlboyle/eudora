# An example client for Eudora's local API (docs/API.md), in Windows PowerShell 5.1 or 7.
#
#   . .\eudora-api.ps1                     # load the functions
#   $r = Request-EudoraAccess -Name "My Report" -Publisher "Me" -Reason "Counts my unread mail" `
#          -Permissions @("mail.headers.read")        # Eudora asks you in its window
#   $token = Receive-EudoraToken $r                    # waits until you answer
#   Invoke-Eudora -Token $token v1.folders.list
#   Invoke-Eudora -Token $token v1.mail.list @{ folder = 12; limit = 20 }
#
# Keep the token somewhere only you can read (it's what lets this program in); Preferences >
# Programs and plugins in Eudora shows what it may do, everything it did, and revokes it.

function Get-EudoraPipeName {
    $user = ($env:USERNAME.ToLower() -replace '[^a-z0-9]', '-')
    "eudora-$user"
}

function Invoke-Eudora {
    param([string]$Token, [Parameter(Mandatory)][string]$Method, [hashtable]$Params = @{})
    $pipe = New-Object System.IO.Pipes.NamedPipeClientStream(".", (Get-EudoraPipeName), [System.IO.Pipes.PipeDirection]::InOut)
    $pipe.Connect(5000)
    try {
        $writer = New-Object System.IO.StreamWriter($pipe, (New-Object System.Text.UTF8Encoding($false)))
        $reader = New-Object System.IO.StreamReader($pipe, [System.Text.Encoding]::UTF8)
        $request = @{ jsonrpc = "2.0"; id = 1; method = $Method; params = $Params }
        if ($Token) { $request.token = $Token }
        $writer.WriteLine(($request | ConvertTo-Json -Depth 10 -Compress))
        $writer.Flush()
        $answer = $reader.ReadLine() | ConvertFrom-Json
        if ($answer.error) { throw "Eudora: $($answer.error.message) (code $($answer.error.code))" }
        $answer.result
    } finally {
        $pipe.Dispose()
    }
}

function Request-EudoraAccess {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][string]$Publisher, [Parameter(Mandatory)][string]$Reason, [Parameter(Mandatory)][string[]]$Permissions)
    $bytes = New-Object byte[] 32
    [System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $secret = [Convert]::ToBase64String($bytes)
    $asked = @($Permissions | ForEach-Object { @{ permission = $_ } })
    $r = Invoke-Eudora -Method v1.auth.request -Params @{ name = $Name; publisher = $Publisher; reason = $Reason; permissions = $asked; secret = $secret }
    [pscustomobject]@{ Request = $r.request; Secret = $secret }
}

function Receive-EudoraToken {
    param([Parameter(Mandatory)]$Asked, [int]$WaitSeconds = 300)
    $until = (Get-Date).AddSeconds($WaitSeconds)
    while ((Get-Date) -lt $until) {
        $r = Invoke-Eudora -Method v1.auth.collect -Params @{ request = $Asked.Request; secret = $Asked.Secret }
        if ($r.state -eq "granted") { return $r.token }
        if ($r.state -eq "refused") { throw "Refused in Eudora." }
        Start-Sleep -Seconds 2
    }
    throw "No answer in Eudora yet."
}
