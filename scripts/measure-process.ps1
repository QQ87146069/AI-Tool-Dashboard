param(
    [Parameter(Mandatory)][int]$ProcessId,
    [int]$Seconds = 30,
    [string]$OutputPath
)
$ErrorActionPreference = 'Stop'

function Get-TreeSample {
    $processes = @(Get-CimInstance Win32_Process -Property ProcessId,ParentProcessId,WorkingSetSize,PrivatePageCount,KernelModeTime,UserModeTime,ReadTransferCount,WriteTransferCount)
    $ids = [System.Collections.Generic.HashSet[uint32]]::new()
    [void]$ids.Add([uint32]$ProcessId)
    do {
        $added = $false
        foreach ($process in $processes) {
            if ($ids.Contains([uint32]$process.ParentProcessId) -and $ids.Add([uint32]$process.ProcessId)) { $added = $true }
        }
    } while ($added)
    $tree = @($processes | Where-Object { $ids.Contains([uint32]$_.ProcessId) })
    if (!($tree | Where-Object ProcessId -eq $ProcessId)) { throw 'Target process has exited' }
    [pscustomobject]@{
        Count = $tree.Count
        Working = ($tree | Measure-Object WorkingSetSize -Sum).Sum
        Private = ($tree | Measure-Object PrivatePageCount -Sum).Sum
        Cpu = (($tree | Measure-Object KernelModeTime -Sum).Sum + ($tree | Measure-Object UserModeTime -Sum).Sum) / 1e7
        Read = ($tree | Measure-Object ReadTransferCount -Sum).Sum
        Write = ($tree | Measure-Object WriteTransferCount -Sum).Sum
    }
}

$rootProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$ProcessId" -Property ExecutablePath
if (!$rootProcess.ExecutablePath) { throw 'Cannot identify target executable' }
$binaryHash = (Get-FileHash -LiteralPath $rootProcess.ExecutablePath -Algorithm SHA256).Hash
$binaryBytes = (Get-Item -LiteralPath $rootProcess.ExecutablePath).Length
$first = Get-TreeSample
$watch = [System.Diagnostics.Stopwatch]::StartNew()
$samples = @($first)
while ($watch.Elapsed.TotalSeconds -lt $Seconds) {
    Start-Sleep -Milliseconds 1000
    $samples += Get-TreeSample
}
$elapsed = $watch.Elapsed.TotalSeconds
$last = $samples[-1]
$result = [pscustomobject]@{
    executableSha256 = $binaryHash
    executableBytes = $binaryBytes
    seconds = [math]::Round($elapsed, 2)
    samples = $samples.Count
    maxProcesses = ($samples | Measure-Object Count -Maximum).Maximum
    avgWorkingSetMiB = [math]::Round(($samples | Measure-Object Working -Average).Average / 1MB, 2)
    maxWorkingSetMiB = [math]::Round(($samples | Measure-Object Working -Maximum).Maximum / 1MB, 2)
    avgPrivateMiB = [math]::Round(($samples | Measure-Object Private -Average).Average / 1MB, 2)
    cpuSeconds = [math]::Round($last.Cpu - $first.Cpu, 5)
    cpuOneCorePercent = [math]::Round(($last.Cpu - $first.Cpu) / $elapsed * 100, 4)
    cpuMachinePercent = [math]::Round(($last.Cpu - $first.Cpu) / $elapsed / [Environment]::ProcessorCount * 100, 4)
    readBytes = $last.Read - $first.Read
    writeBytes = $last.Write - $first.Write
}
$json = $result | ConvertTo-Json
if ($OutputPath) { $json | Set-Content -LiteralPath $OutputPath -Encoding utf8 }
$json
