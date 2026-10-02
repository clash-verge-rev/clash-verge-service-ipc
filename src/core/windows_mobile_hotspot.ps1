$ErrorActionPreference = 'Stop'
$request = $env:CLASH_VERGE_HOTSPOT_REQUEST | ConvertFrom-Json
$backupPath = $env:CLASH_VERGE_HOTSPOT_BACKUP
$action = [string]$env:CLASH_VERGE_HOTSPOT_ACTION
if ([string]::IsNullOrWhiteSpace($action)) { $action = 'reconcile' }

function Get-IcsConnections {
  $manager = New-Object -ComObject HNetCfg.HNetShare
  @($manager.EnumEveryConnection()) | ForEach-Object {
    $connection = $_
    $props = $manager.NetConnectionProps($connection)
    $config = $manager.INetSharingConfigurationForINetConnection($connection)
    [pscustomobject]@{
      Connection = $connection
      Config = $config
      Guid = [string]$props.Guid
      Name = [string]$props.Name
      DeviceName = [string]$props.DeviceName
      Status = [int]$props.Status
      SharingEnabled = [bool]$config.SharingEnabled
      SharingType = if ($config.SharingEnabled) { [int]$config.SharingConnectionType } else { -1 }
    }
  }
}

function Get-FreshIcsRolesByGuid([string[]]$guids) {
  # Cached INetSharingConfiguration objects can outlive a removed Mobile
  # Hotspot adapter and continue reporting its old PRIVATE role. Re-enumerate
  # HNetCfg when checking lifecycle boundaries, but only request a sharing
  # configuration for the exact GUIDs the caller needs.
  $wanted = @{}
  foreach ($guid in $guids) {
    if (-not [string]::IsNullOrWhiteSpace($guid)) {
      $wanted[$guid] = $true
    }
  }
  $roles = @{}
  if ($wanted.Count -eq 0) { return $roles }

  $manager = New-Object -ComObject HNetCfg.HNetShare
  foreach ($connection in @($manager.EnumEveryConnection())) {
    $props = $manager.NetConnectionProps($connection)
    $guid = [string]$props.Guid
    if (-not $wanted.ContainsKey($guid)) { continue }

    $config = $manager.INetSharingConfigurationForINetConnection($connection)
    $roles[$guid] = [pscustomobject]@{
      Config = $config
      SharingEnabled = [bool]$config.SharingEnabled
      SharingType = if ($config.SharingEnabled) { [int]$config.SharingConnectionType } else { -1 }
    }
  }
  return $roles
}

function Find-ByGuid($connections, [string]$guid) {
  @($connections | Where-Object { $_.Guid -eq $guid })
}

function Get-PublicConnections($connections) {
  @($connections | Where-Object { $_.SharingEnabled -and $_.SharingType -eq 0 })
}

function Get-PrivateConnections($connections) {
  @($connections | Where-Object { $_.SharingEnabled -and $_.SharingType -eq 1 })
}

function Get-PublicGuid($connections) {
  $public = @(Get-PublicConnections $connections)
  if ($public.Count -gt 1) {
    throw "Expected at most one ICS public adapter, found $($public.Count)."
  }
  if ($public.Count -eq 1) { return [string]$public[0].Guid }
  return $null
}

function Test-IsPublic($connections, [string]$guid) {
  if ([string]::IsNullOrWhiteSpace($guid)) { return $false }
  $target = @(Find-ByGuid $connections $guid)
  $target.Count -eq 1 -and $target[0].SharingEnabled -and $target[0].SharingType -eq 0
}

function Test-IsPrivate($connections, [string]$guid) {
  if ([string]::IsNullOrWhiteSpace($guid)) { return $false }
  $target = @(Find-ByGuid $connections $guid)
  $target.Count -eq 1 -and $target[0].SharingEnabled -and $target[0].SharingType -eq 1
}

function Test-IsOwnedTopology($connections, $snapshot) {
  (Test-IsPublic $connections ([string]$snapshot.appliedPublic)) -and
    (Test-IsPrivate $connections ([string]$snapshot.appliedPrivate))
}

function Get-IcsStaRunspace([string]$kind) {
  if ($null -eq $global:ClashVergeIcsStaRunspaces) {
    $global:ClashVergeIcsStaRunspaces = @{}
  }
  $runspace = $global:ClashVergeIcsStaRunspaces[$kind]
  if ($null -eq $runspace -or $runspace.RunspaceStateInfo.State -ne 'Opened') {
    if ($null -ne $runspace) { $runspace.Dispose() }
    $runspace = [RunspaceFactory]::CreateRunspace()
    $runspace.ApartmentState = [Threading.ApartmentState]::STA
    $runspace.ThreadOptions = [Management.Automation.Runspaces.PSThreadOptions]::ReuseThread
    $runspace.Open()
    $global:ClashVergeIcsStaRunspaces[$kind] = $runspace
  }
  $runspace
}

function Initialize-IcsStaPool {
  foreach ($kind in @('source', 'target', 'private')) {
    Get-IcsStaRunspace $kind | Out-Null
  }
}

function Set-IcsBinding([string]$kind, [string]$guid, $config) {
  if ($null -eq $global:ClashVergeIcsBindings) {
    $global:ClashVergeIcsBindings = @{}
  }
  $key = $guid.Trim('{}').ToLowerInvariant()
  $global:ClashVergeIcsBindings[$key] = [pscustomobject]@{
    Guid = $guid
    Config = $config
  }
}

function Get-IcsBinding([string]$kind, [string]$guid, $connections) {
  # A caller that already paid for a fresh HNetCfg enumeration must refresh
  # the cached COM object as well. This keeps the fast path from reusing an
  # adapter generation that Windows has since replaced under the same GUID.
  if ($null -ne $connections) {
    $matches = @(Find-ByGuid $connections $guid)
    if ($matches.Count -ne 1) { return $null }
    Set-IcsBinding $kind $guid $matches[0].Config
    return $matches[0].Config
  }
  if ($null -ne $global:ClashVergeIcsBindings) {
    $key = $guid.Trim('{}').ToLowerInvariant()
    $binding = $global:ClashVergeIcsBindings[$key]
    if ($null -ne $binding -and [string]$binding.Guid -eq $guid -and $null -ne $binding.Config) {
      return $binding.Config
    }
  }
  return $null
}

function Test-IcsBinding([string]$guid) {
  if ([string]::IsNullOrWhiteSpace($guid) -or $null -eq $global:ClashVergeIcsBindings) { return $false }
  $key = $guid.Trim('{}').ToLowerInvariant()
  $binding = $global:ClashVergeIcsBindings[$key]
  $null -ne $binding -and $null -ne $binding.Config
}

function Test-IcsConfigRole($config, [int]$role) {
  try {
    [bool]$config.SharingEnabled -and [int]$config.SharingConnectionType -eq $role
  } catch {
    $false
  }
}

function Remove-IcsBinding([string]$guid) {
  if ([string]::IsNullOrWhiteSpace($guid) -or $null -eq $global:ClashVergeIcsBindings) { return }
  $key = $guid.Trim('{}').ToLowerInvariant()
  $global:ClashVergeIcsBindings.Remove($key)
}

function Start-IcsStaWorker([scriptblock]$script, [object[]]$arguments) {
  $kind = [string]$arguments[0]
  $runspace = Get-IcsStaRunspace $kind
  $powershell = [PowerShell]::Create()
  $powershell.Runspace = $runspace
  try {
    $pipeline = $powershell.AddScript($script.ToString())
    foreach ($argument in $arguments) {
      $pipeline = $pipeline.AddArgument($argument)
    }
    [pscustomobject]@{
      Runspace = $runspace
      PowerShell = $powershell
      Async = $powershell.BeginInvoke()
    }
  } catch {
    $powershell.Dispose()
    throw
  }
}

function Stop-IcsStaWorkers([object[]]$workers) {
  foreach ($worker in @($workers)) {
    if ($null -eq $worker) { continue }
    try {
      if (-not $worker.Async.IsCompleted) { $worker.PowerShell.Stop() }
    } finally {
      $worker.PowerShell.Dispose()
    }
  }
}

function Set-IcsPair(
  [string]$publicGuid,
  [string]$privateGuid,
  [string]$expectedCurrentPublicGuid,
  $connections,
  [bool]$forceRebuild = $false
) {
  $prepareStarted = [Diagnostics.Stopwatch]::StartNew()
  Initialize-IcsStaPool

  $sourceConfig = if ([string]::IsNullOrWhiteSpace($expectedCurrentPublicGuid)) {
    $null
  } else {
    Get-IcsBinding 'source' $expectedCurrentPublicGuid $connections
  }
  $targetConfig = if ([string]::IsNullOrWhiteSpace($publicGuid)) {
    $null
  } else {
    Get-IcsBinding 'target' $publicGuid $connections
  }
  $privateConfig = if ([string]::IsNullOrWhiteSpace($privateGuid)) {
    $null
  } else {
    Get-IcsBinding 'private' $privateGuid $connections
  }
  if (-not [string]::IsNullOrWhiteSpace($expectedCurrentPublicGuid) -and $null -eq $sourceConfig) {
    throw "Cannot bind expected current ICS public adapter $expectedCurrentPublicGuid."
  }
  if (-not [string]::IsNullOrWhiteSpace($publicGuid) -and $null -eq $targetConfig) {
    throw "Cannot bind target ICS public adapter $publicGuid."
  }
  if (-not [string]::IsNullOrWhiteSpace($privateGuid) -and $null -eq $privateConfig) {
    throw "Cannot bind ICS private adapter $privateGuid."
  }

  $disableStart = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $enableStart = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $sourceReady = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $targetReady = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $privateReady = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $sourceDisableDone = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $privateDisableDone = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $targetEnableDone = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $privateEnableDone = [Threading.EventWaitHandle]::new($false, [Threading.EventResetMode]::ManualReset)
  $results = [Collections.Concurrent.ConcurrentDictionary[string, object]]::new()

  $workerScript = {
    param(
      [string]$kind,
      $config,
      [Threading.EventWaitHandle]$disableStart,
      [Threading.EventWaitHandle]$enableStart,
      [Threading.EventWaitHandle]$ready,
      [Threading.EventWaitHandle]$disableDone,
      [Threading.EventWaitHandle]$enableDone,
      [Collections.Concurrent.ConcurrentDictionary[string, object]]$results
    )
    $ErrorActionPreference = 'Stop'
    function Test-Role($candidate, [int]$role) {
      try {
        [bool]$candidate.SharingEnabled -and [int]$candidate.SharingConnectionType -eq $role
      } catch {
        $false
      }
    }
    try {
      $role = if ($kind -eq 'private') { 1 } else { 0 }
      $results["$kind.prepare"] = [pscustomobject]@{
        Success = $true
        RoleMatches = Test-Role $config $role
        Error = $null
      }
    } catch {
      $results["$kind.prepare"] = [pscustomobject]@{ Success = $false; RoleMatches = $false; Error = $_.Exception.ToString() }
    } finally {
      $ready.Set() | Out-Null
    }

    if ($kind -eq 'source') {
      if (-not $disableStart.WaitOne([TimeSpan]::FromSeconds(60))) { return }
      if ([bool]$results['cancel']) { return }
      $stopwatch = [Diagnostics.Stopwatch]::StartNew()
      try {
        $config.DisableSharing()
        $results['source.disable'] = [pscustomobject]@{ Success = $true; CallMs = [math]::Round($stopwatch.Elapsed.TotalMilliseconds, 1); Error = $null }
      } catch {
        $results['source.disable'] = [pscustomobject]@{ Success = $false; CallMs = [math]::Round($stopwatch.Elapsed.TotalMilliseconds, 1); Error = $_.Exception.ToString() }
      } finally {
        $disableDone.Set() | Out-Null
      }
      return
    }

    if ($kind -eq 'private') {
      if (-not $disableStart.WaitOne([TimeSpan]::FromSeconds(60))) { return }
      if ([bool]$results['cancel']) { return }
      $disableStopwatch = [Diagnostics.Stopwatch]::StartNew()
      $disableSucceeded = $false
      try {
        $config.DisableSharing()
        $disableSucceeded = $true
        $results['private.disable'] = [pscustomobject]@{ Success = $true; CallMs = [math]::Round($disableStopwatch.Elapsed.TotalMilliseconds, 1); Error = $null }
      } catch {
        $results['private.disable'] = [pscustomobject]@{ Success = $false; CallMs = [math]::Round($disableStopwatch.Elapsed.TotalMilliseconds, 1); Error = $_.Exception.ToString() }
      } finally {
        $disableDone.Set() | Out-Null
      }
      if (-not $disableSucceeded -or -not $enableStart.WaitOne([TimeSpan]::FromSeconds(60))) { return }
      if ([bool]$results['cancel']) { return }
      $enableStopwatch = [Diagnostics.Stopwatch]::StartNew()
      try {
        $config.EnableSharing(1)
        $results['private.enable'] = [pscustomobject]@{
          Success = $true
          Verified = Test-Role $config 1
          CallMs = [math]::Round($enableStopwatch.Elapsed.TotalMilliseconds, 1)
          Error = $null
        }
      } catch {
        $results['private.enable'] = [pscustomobject]@{ Success = $false; Verified = $false; CallMs = [math]::Round($enableStopwatch.Elapsed.TotalMilliseconds, 1); Error = $_.Exception.ToString() }
      } finally {
        $enableDone.Set() | Out-Null
      }
      return
    }

    if ($kind -eq 'target') {
      if (-not $enableStart.WaitOne([TimeSpan]::FromSeconds(60))) { return }
      if ([bool]$results['cancel']) { return }
      $stopwatch = [Diagnostics.Stopwatch]::StartNew()
      try {
        $config.EnableSharing(0)
        $results['target.enable'] = [pscustomobject]@{
          Success = $true
          Verified = Test-Role $config 0
          CallMs = [math]::Round($stopwatch.Elapsed.TotalMilliseconds, 1)
          Error = $null
        }
      } catch {
        $results['target.enable'] = [pscustomobject]@{ Success = $false; Verified = $false; CallMs = [math]::Round($stopwatch.Elapsed.TotalMilliseconds, 1); Error = $_.Exception.ToString() }
      } finally {
        $enableDone.Set() | Out-Null
      }
    }
  }

  $workers = @()
  try {
    if ($null -ne $sourceConfig) {
      $workers += Start-IcsStaWorker $workerScript @('source', $sourceConfig, $disableStart, $enableStart, $sourceReady, $sourceDisableDone, $null, $results)
    } else {
      $results['source.prepare'] = [pscustomobject]@{ Success = $true; RoleMatches = $true; Error = $null }
      $sourceReady.Set() | Out-Null
      $sourceDisableDone.Set() | Out-Null
    }
    if ($null -ne $targetConfig) {
      $workers += Start-IcsStaWorker $workerScript @('target', $targetConfig, $disableStart, $enableStart, $targetReady, $null, $targetEnableDone, $results)
    } else {
      $results['target.prepare'] = [pscustomobject]@{ Success = $true; RoleMatches = $false; Error = $null }
      $targetReady.Set() | Out-Null
      $targetEnableDone.Set() | Out-Null
    }
    if ($null -ne $privateConfig) {
      $workers += Start-IcsStaWorker $workerScript @('private', $privateConfig, $disableStart, $enableStart, $privateReady, $privateDisableDone, $privateEnableDone, $results)
    } else {
      $results['private.prepare'] = [pscustomobject]@{ Success = $true; RoleMatches = $true; Error = $null }
      $privateReady.Set() | Out-Null
      $privateDisableDone.Set() | Out-Null
      $privateEnableDone.Set() | Out-Null
    }

    foreach ($ready in @($sourceReady, $targetReady, $privateReady)) {
      if (-not $ready.WaitOne([TimeSpan]::FromSeconds(30))) { throw 'Timed out preparing an ICS STA worker.' }
    }
    foreach ($kind in @('source', 'target', 'private')) {
      if (-not $results["$kind.prepare"].Success) {
        throw "Failed to prepare the $kind ICS STA worker: $($results["$kind.prepare"].Error)"
      }
    }
    $prepareMs = [math]::Round($prepareStarted.Elapsed.TotalMilliseconds, 1)

    $sourceOwned = [bool]$results['source.prepare'].RoleMatches
    $targetAlreadyPublic = [bool]$results['target.prepare'].RoleMatches
    $privateOwned = [bool]$results['private.prepare'].RoleMatches
    if ($targetAlreadyPublic -and $privateOwned -and -not $forceRebuild) {
      return [pscustomobject]@{ Owned = $true; Changed = $false; PrepareMs = $prepareMs; DisableWallMs = 0; EnableWallMs = 0 }
    }
    if (-not $sourceOwned -or -not $privateOwned) {
      return [pscustomobject]@{ Owned = $false; Changed = $false; PrepareMs = $prepareMs; DisableWallMs = 0; EnableWallMs = 0 }
    }

    $disableStarted = [Diagnostics.Stopwatch]::StartNew()
    $disableStart.Set() | Out-Null
    if (-not $sourceDisableDone.WaitOne([TimeSpan]::FromSeconds(30)) -or
        -not $privateDisableDone.WaitOne([TimeSpan]::FromSeconds(30))) {
      throw 'Concurrent ICS DisableSharing phase timed out.'
    }
    if ($null -ne $sourceConfig -and -not $results['source.disable'].Success) {
      throw "Failed to disable the previous ICS public adapter: $($results['source.disable'].Error)"
    }
    if ($null -ne $privateConfig -and -not $results['private.disable'].Success) {
      throw "Failed to disable the ICS private adapter: $($results['private.disable'].Error)"
    }
    $disableWallMs = [math]::Round($disableStarted.Elapsed.TotalMilliseconds, 1)

    $enableStarted = [Diagnostics.Stopwatch]::StartNew()
    $enableStart.Set() | Out-Null
    if (-not $targetEnableDone.WaitOne([TimeSpan]::FromSeconds(30)) -or
        -not $privateEnableDone.WaitOne([TimeSpan]::FromSeconds(30))) {
      throw 'Concurrent ICS EnableSharing phase timed out.'
    }
    if ($null -ne $targetConfig -and
        (-not $results['target.enable'].Success -or -not $results['target.enable'].Verified)) {
      throw "Failed to establish the target ICS public adapter: $($results['target.enable'].Error)"
    }
    if ($null -ne $privateConfig -and
        (-not $results['private.enable'].Success -or -not $results['private.enable'].Verified)) {
      throw "Failed to re-establish the ICS private adapter: $($results['private.enable'].Error)"
    }
    [pscustomobject]@{
      Owned = $true
      Changed = $true
      PrepareMs = $prepareMs
      DisableWallMs = $disableWallMs
      EnableWallMs = [math]::Round($enableStarted.Elapsed.TotalMilliseconds, 1)
      SourceDisableCallMs = if ($null -ne $sourceConfig) { $results['source.disable'].CallMs } else { 0 }
      PrivateDisableCallMs = if ($null -ne $privateConfig) { $results['private.disable'].CallMs } else { 0 }
      TargetEnableCallMs = if ($null -ne $targetConfig) { $results['target.enable'].CallMs } else { 0 }
      PrivateEnableCallMs = if ($null -ne $privateConfig) { $results['private.enable'].CallMs } else { 0 }
    }
  } finally {
    $results['cancel'] = $true
    $disableStart.Set() | Out-Null
    $enableStart.Set() | Out-Null
    Stop-IcsStaWorkers $workers
    foreach ($event in @($disableStart, $enableStart, $sourceReady, $targetReady, $privateReady,
        $sourceDisableDone, $privateDisableDone, $targetEnableDone, $privateEnableDone)) {
      $event.Dispose()
    }
  }
}

function Save-Snapshot($snapshot) {
  $backupDirectory = Split-Path -Parent $backupPath
  New-Item -ItemType Directory -Path $backupDirectory -Force | Out-Null
  $temporary = "$backupPath.tmp"
  $snapshot | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $temporary -Encoding UTF8
  Move-Item -LiteralPath $temporary -Destination $backupPath -Force
}

function Load-Snapshot {
  $snapshot = Get-Content -LiteralPath $backupPath -Raw | ConvertFrom-Json
  if ([int]$snapshot.version -ne 1 -or [string]$snapshot.mode -ne 'paired') {
    throw 'Unsupported Mobile Hotspot compatibility backup version.'
  }
  if ($null -eq $snapshot.PSObject.Properties['abandoned']) {
    $snapshot | Add-Member -NotePropertyName abandoned -NotePropertyValue $false
  }
  return $snapshot
}

function Restore-IcsPair($snapshot, $connections = $null) {
  if ($null -eq $connections) { $connections = Get-IcsConnections }
  $previousPublic = [string]$snapshot.previousPublic
  # PowerShell unwraps a single pipeline result to a scalar.  Accessing
  # `.Count` on that scalar is not reliable for every object shape and caused
  # an existing hotspot adapter to be treated as missing.  Materialize the
  # result before checking its cardinality so restoration always recreates the
  # complete PUBLIC/PRIVATE pair when the saved private adapter still exists.
  $privateMatches = @(Find-ByGuid $connections ([string]$snapshot.appliedPrivate))
  $privateGuid = if ($privateMatches.Count -eq 1) {
    [string]$snapshot.appliedPrivate
  } else {
    $null
  }
  $transition = Set-IcsPair $previousPublic $privateGuid ([string]$snapshot.appliedPublic) $connections
  return $transition
}

function Write-IcsResult([string]$outcome) {
  $observed = Get-IcsConnections
  $publicGuids = @(@(Get-PublicConnections $observed) | ForEach-Object { [string]$_.Guid })
  $privateGuids = @(@(Get-PrivateConnections $observed) | ForEach-Object { [string]$_.Guid })
  @{
    outcome = $outcome
    public = $publicGuids
    private = $privateGuids
  } | ConvertTo-Json -Compress
}

if ($action -eq 'prewarm') {
  $started = [Diagnostics.Stopwatch]::StartNew()
  if ($null -ne $global:ClashVergeIcsPrewarmTransition) {
    @{
      outcome = 'waiting_for_hotspot'
      transition = @{
        PrewarmMs = [math]::Round($started.Elapsed.TotalMilliseconds, 1)
        Cached = $true
        PublicGuid = $global:ClashVergeIcsPrewarmTransition.PublicGuid
        TunnelGuid = $global:ClashVergeIcsPrewarmTransition.TunnelGuid
      }
    } | ConvertTo-Json -Depth 5 -Compress
    return
  }
  Initialize-IcsStaPool
  $prewarmConnections = Get-IcsConnections
  $prewarmTunnels = @($prewarmConnections | Where-Object {
    $_.Status -eq 2 -and
    ($_.Name -eq [string]$request.tun_device_name -or $_.DeviceName -eq 'Meta Tunnel')
  })
  if ($prewarmTunnels.Count -ne 1) {
    throw "Expected exactly one connected Mihomo TUN adapter while prewarming, found $($prewarmTunnels.Count)."
  }
  $prewarmPublic = @(Get-PublicConnections $prewarmConnections)
  if ($prewarmPublic.Count -gt 1) {
    throw "Expected at most one ICS public adapter while prewarming, found $($prewarmPublic.Count)."
  }
  Set-IcsBinding 'target' ([string]$prewarmTunnels[0].Guid) $prewarmTunnels[0].Config
  if ($prewarmPublic.Count -eq 1) {
    Set-IcsBinding 'source' ([string]$prewarmPublic[0].Guid) $prewarmPublic[0].Config
  }
  $global:ClashVergeIcsPrewarmTransition = [pscustomobject]@{
    PublicGuid = if ($prewarmPublic.Count -eq 1) { [string]$prewarmPublic[0].Guid } else { $null }
    TunnelGuid = [string]$prewarmTunnels[0].Guid
  }
  @{
    outcome = 'waiting_for_hotspot'
    transition = @{
      PrewarmMs = [math]::Round($started.Elapsed.TotalMilliseconds, 1)
      Cached = $false
      PublicGuid = $global:ClashVergeIcsPrewarmTransition.PublicGuid
      TunnelGuid = $global:ClashVergeIcsPrewarmTransition.TunnelGuid
    }
  } | ConvertTo-Json -Depth 5 -Compress
  return
}

if ($action.StartsWith('inspect_private_role:', [StringComparison]::Ordinal)) {
  $privateGuid = $action.Substring('inspect_private_role:'.Length)
  $freshRoles = Get-FreshIcsRolesByGuid @($privateGuid)
  $privateRole = $freshRoles[$privateGuid]
  $isPrivate = $null -ne $privateRole -and
    [bool]$privateRole.SharingEnabled -and
    [int]$privateRole.SharingType -eq 1
  if ($isPrivate) {
    Set-IcsBinding 'private' $privateGuid $privateRole.Config
  } else {
    Remove-IcsBinding $privateGuid | Out-Null
  }
  $privateGuids = @()
  if ($isPrivate) { $privateGuids = @($privateGuid) }
  @{
    outcome = 'unchanged'
    public = @()
    private = $privateGuids
  } | ConvertTo-Json -Compress
  return
}

if ($action.StartsWith('inspect_stopped:', [StringComparison]::Ordinal)) {
  $parts = $action.Substring('inspect_stopped:'.Length).Split('|')
  if ($parts.Count -ne 2) { throw 'Invalid stopped-hotspot inspection request.' }
  $publicGuid = [string]$parts[0]
  $privateGuid = [string]$parts[1]
  $freshRoles = Get-FreshIcsRolesByGuid @($publicGuid, $privateGuid)
  $publicRole = $freshRoles[$publicGuid]
  $privateRole = $freshRoles[$privateGuid]
  $isPublic = $null -ne $publicRole -and
    [bool]$publicRole.SharingEnabled -and
    [int]$publicRole.SharingType -eq 0
  $isPrivate = $null -ne $privateRole -and
    [bool]$privateRole.SharingEnabled -and
    [int]$privateRole.SharingType -eq 1
  if ($null -ne $publicRole) {
    Set-IcsBinding 'target' $publicGuid $publicRole.Config
  } else {
    Remove-IcsBinding $publicGuid | Out-Null
  }
  if ($null -ne $privateRole) {
    Set-IcsBinding 'private' $privateGuid $privateRole.Config
  } else {
    Remove-IcsBinding $privateGuid | Out-Null
  }
  $publicGuids = @()
  $privateGuids = @()
  if ($isPublic) { $publicGuids = @($publicGuid) }
  if ($isPrivate) { $privateGuids = @($privateGuid) }
  @{
    outcome = 'unchanged'
    public = $publicGuids
    private = $privateGuids
  } | ConvertTo-Json -Compress
  return
}

function Restore-LivePair($snapshot, $connections = $null) {
  if (-not [bool]$snapshot.active) { return $null }

  $previousPublic = [string]$snapshot.previousPublic
  $appliedPublic = [string]$snapshot.appliedPublic
  $expectedPrivate = [string]$snapshot.appliedPrivate

  $targetConfig = Get-IcsBinding 'target' $previousPublic $connections
  $privateConfig = Get-IcsBinding 'private' $expectedPrivate $connections
  $targetPairIsLive = $null -ne $targetConfig -and
    $null -ne $privateConfig -and
    (Test-IcsConfigRole $targetConfig 0) -and
    (Test-IcsConfigRole $privateConfig 1)

  if ($targetPairIsLive) {
    # A stopped-hotspot recovery first recreates a temporary WLAN-backed
    # session. Windows may report the desired WLAN+PRIVATE roles immediately,
    # but merely observing those roles is not enough: the pair must be rebuilt
    # while the hotspot is On before WinRT stops it, otherwise Windows can
    # discard the restored PUBLIC role during teardown.
    $transition = Set-IcsPair $previousPublic $expectedPrivate $previousPublic $connections $true
  } else {
    # Only take over the exact Mihomo+saved-PRIVATE pair. A third-party PUBLIC
    # or PRIVATE change is not owned by this feature and must remain untouched.
    $transition = Set-IcsPair $previousPublic $expectedPrivate $appliedPublic $connections
  }
  return $transition
}

if ($action -eq 'inspect') {
  Write-IcsResult 'unchanged'
  return
}

if ($action -eq 'restore_live') {
  if (-not (Test-Path -LiteralPath $backupPath)) {
    Write-IcsResult 'unchanged'
    return
  }
  $snapshot = Load-Snapshot
  $restoreConnections = $null
  $hasAppliedBinding = Test-IcsBinding ([string]$snapshot.appliedPublic)
  $hasPrivateBinding = Test-IcsBinding ([string]$snapshot.appliedPrivate)
  $previousPublic = [string]$snapshot.previousPublic
  $hasPreviousBinding = [string]::IsNullOrWhiteSpace($previousPublic) -or (Test-IcsBinding $previousPublic)
  if (-not $hasAppliedBinding -or -not $hasPrivateBinding -or -not $hasPreviousBinding) {
    # Service restart or adapter generation change: rebuild the bindings once.
    # The normal live path uses the prewarmed objects and skips this enumeration.
    $restoreConnections = Get-IcsConnections
  }
  $transition = Restore-LivePair $snapshot $restoreConnections
  $restored = $null -ne $transition -and [bool]$transition.Owned
  # Restore-LivePair already performs a complete post-mutation verification.
  # Its caller performs another state inspection after stopping the temporary
  # hotspot, so enumerating HNetCfg again only to construct this response adds
  # latency without establishing a new safety condition.
  @{ outcome = if ($restored) { 'restored' } else { 'unchanged' }; transition = $transition } | ConvertTo-Json -Depth 5 -Compress
  return
}

# The steady-state monitor normally needs only the three objects captured when
# the pair was established.  Avoid paying for EnumEveryConnection on every
# observation while either our exact Mihomo+hotspot pair, or the exact saved
# WLAN+hotspot pair seen during a Windows transition, is still present.  Any
# ambiguous role change falls through to the full snapshot below so an empty
# PUBLIC transition can still be distinguished from an unrelated third-party
# PUBLIC adapter before ownership is abandoned.
if ([bool]$request.enabled -and (Test-Path -LiteralPath $backupPath)) {
  $fastSnapshot = Load-Snapshot
  if ([bool]$fastSnapshot.active) {
    $appliedPublicGuid = [string]$fastSnapshot.appliedPublic
    $appliedPrivateGuid = [string]$fastSnapshot.appliedPrivate
    $previousPublicGuid = [string]$fastSnapshot.previousPublic
    $hasFastBindings =
      (Test-IcsBinding $appliedPublicGuid) -and
      (Test-IcsBinding $appliedPrivateGuid) -and
      ([string]::IsNullOrWhiteSpace($previousPublicGuid) -or (Test-IcsBinding $previousPublicGuid))
    if ($hasFastBindings) {
      $appliedPublicConfig = Get-IcsBinding 'target' $appliedPublicGuid $null
      $appliedPrivateConfig = Get-IcsBinding 'private' $appliedPrivateGuid $null
      $ownedPair =
        (Test-IcsConfigRole $appliedPublicConfig 0) -and
        (Test-IcsConfigRole $appliedPrivateConfig 1)
      $savedPair = $false
      if (-not [string]::IsNullOrWhiteSpace($previousPublicGuid)) {
        $previousPublicConfig = Get-IcsBinding 'source' $previousPublicGuid $null
        $savedPair =
          (Test-IcsConfigRole $previousPublicConfig 0) -and
          (Test-IcsConfigRole $appliedPrivateConfig 1)
      }
      if ($ownedPair -or $savedPair) {
        @{
          outcome = 'unchanged'
          transition = @{
            FastPath = $true
            OwnedPair = $ownedPair
            SavedPair = $savedPair
          }
        } | ConvertTo-Json -Depth 5 -Compress
        return
      }
    }
  }
}

$connections = Get-IcsConnections
$hotspots = @($connections | Where-Object {
  $_.Status -eq 2 -and
  $_.DeviceName -like 'Microsoft Wi-Fi Direct Virtual Adapter*' -and
  $_.SharingEnabled -and $_.SharingType -eq 1
})
$hotspotRunning = $hotspots.Count -gt 0

if (-not [bool]$request.enabled) {
  if (-not (Test-Path -LiteralPath $backupPath)) {
    @{ outcome = 'unchanged' } | ConvertTo-Json -Compress
    return
  }
  $snapshot = Load-Snapshot
  $transition = if ([bool]$snapshot.active) { Restore-IcsPair $snapshot $connections } else { $null }
  $restored = $null -ne $transition -and [bool]$transition.Owned
  Remove-Item -LiteralPath $backupPath -Force
  @{ outcome = if ($restored) { 'restored' } else { 'unchanged' }; transition = $transition } | ConvertTo-Json -Depth 5 -Compress
  return
}

if (-not $hotspotRunning) {
  if (Test-Path -LiteralPath $backupPath) {
    $snapshot = Load-Snapshot
    if ([bool]$snapshot.active) {
      # The Rust coordinator calls this reconcile action only after WinRT has
      # reported that the hotspot is On. If HNetCfg has not exposed its PRIVATE
      # side yet, retain the snapshot and wait for the next observation. Actual
      # stopped-hotspot restoration is handled by the WinRT Off path, which
      # waits for the exact saved PRIVATE GUID to disappear before mutating ICS.
      @{ outcome = 'waiting_for_hotspot' } | ConvertTo-Json -Compress
      return
    }
    if ([bool]$snapshot.abandoned) {
      $snapshot.abandoned = $false
      Save-Snapshot $snapshot
    }
  }
  @{ outcome = 'waiting_for_hotspot' } | ConvertTo-Json -Compress
  return
}

$tunnels = @($connections | Where-Object {
  $_.Status -eq 2 -and
  ($_.Name -eq [string]$request.tun_device_name -or $_.DeviceName -eq 'Meta Tunnel')
})
if ($tunnels.Count -ne 1) {
  throw "Expected exactly one connected Mihomo TUN adapter, found $($tunnels.Count)."
}
$tunnelGuid = [string]$tunnels[0].Guid
$privateConnections = @(Get-PrivateConnections $connections)
if ($privateConnections.Count -ne 1 -or $hotspots.Count -ne 1 -or
    [string]$privateConnections[0].Guid -ne [string]$hotspots[0].Guid) {
  throw "Expected exactly one active Mobile Hotspot ICS private adapter, found $($privateConnections.Count) private adapter(s) and $($hotspots.Count) active hotspot adapter(s)."
}
$hotspotGuid = [string]$hotspots[0].Guid

if (Test-Path -LiteralPath $backupPath) {
  $snapshot = Load-Snapshot
  if ([bool]$snapshot.active) {
    if (Test-IsOwnedTopology $connections $snapshot) {
      @{ outcome = 'unchanged' } | ConvertTo-Json -Compress
      return
    }
    $publicConnections = @(Get-PublicConnections $connections)
    $previousPublic = [string]$snapshot.previousPublic
    if (-not [string]::IsNullOrWhiteSpace($previousPublic) -and
        (Test-IsPublic $connections $previousPublic) -and
        (Test-IsPrivate $connections ([string]$snapshot.appliedPrivate))) {
      # During hotspot shutdown Windows can restore the original PUBLIC before
      # the Wi-Fi Direct PRIVATE adapter stops reporting as active. This is the
      # exact saved topology, not an unknown third-party replacement. Keep the
      # snapshot until the next observation confirms whether the hotspot really
      # stopped; do not mutate ICS or relinquish restoration ownership here.
      @{ outcome = 'unchanged' } | ConvertTo-Json -Compress
      return
    }
    if ((Test-IsPrivate $connections ([string]$snapshot.appliedPrivate)) -and
        $publicConnections.Count -eq 0) {
      # During Mobile Hotspot shutdown Windows can remove PUBLIC shortly before
      # it removes the Wi-Fi Direct PRIVATE side, and separate HNetCfg clients
      # can observe those changes at different times. An empty PUBLIC set is
      # therefore indeterminate: retain the snapshot without mutating ICS. A
      # later no-hotspot observation restores the saved PUBLIC, while an
      # explicitly different PUBLIC below still relinquishes ownership.
      @{ outcome = 'unchanged' } | ConvertTo-Json -Compress
      return
    }
    # Either side changed while the hotspot remained active. Relinquish
    # ownership: do not fight the new topology and do not restore our snapshot.
    $observedSharing = @($connections | Where-Object { $_.SharingEnabled } | ForEach-Object {
      [pscustomobject]@{
        guid = [string]$_.Guid
        name = [string]$_.Name
        deviceName = [string]$_.DeviceName
        status = [int]$_.Status
        sharingType = [int]$_.SharingType
      }
    })
    $snapshot | Add-Member -Force -NotePropertyName abandonObservedAt -NotePropertyValue ((Get-Date).ToString('o'))
    $snapshot | Add-Member -Force -NotePropertyName abandonObservedSharing -NotePropertyValue $observedSharing
    $snapshot.active = $false
    $snapshot.abandoned = $true
    Save-Snapshot $snapshot
    @{ outcome = 'unchanged' } | ConvertTo-Json -Compress
    return
  }
  if ([bool]$snapshot.abandoned) {
    @{ outcome = 'unchanged' } | ConvertTo-Json -Compress
    return
  }
} else {
  $snapshot = [pscustomobject]@{
    version = 1
    mode = 'paired'
    active = $false
    appliedPublic = $tunnelGuid
    appliedPrivate = $hotspotGuid
    previousPublic = $null
    abandoned = $false
  }
}

# A new hotspot session starts a new ownership window. Windows can expose the
# hotspot PRIVATE side before it has restored the ordinary PUBLIC side. Do not
# overwrite a previously captured PUBLIC GUID with that transient empty state.
# A non-empty, non-Mihomo PUBLIC is a real new baseline and replaces the saved
# value; a brand-new snapshot naturally keeps null when no PUBLIC has ever been
# observed.
$currentPublic = Get-PublicGuid $connections
if (-not [string]::IsNullOrWhiteSpace($currentPublic) -and
    $currentPublic -ne $tunnelGuid) {
  $snapshot.previousPublic = $currentPublic
}
$snapshot.appliedPublic = $tunnelGuid
$snapshot.appliedPrivate = $hotspotGuid
$snapshot.mode = 'paired'
$snapshot.active = $true
$snapshot.abandoned = $false
Save-Snapshot $snapshot

try {
  # HNetCfg cannot reliably replace the PUBLIC side of a live ICS pair in
  # place. Clear the existing pair first, then recreate the complete pair.
  $transition = Set-IcsPair $tunnelGuid $hotspotGuid $currentPublic $connections
  if (-not [bool]$transition.Owned) {
    $snapshot.active = $false
    $snapshot.abandoned = $true
    Save-Snapshot $snapshot
    @{ outcome = 'unchanged' } | ConvertTo-Json -Compress
    return
  }
} catch {
  $applyError = $_
  try {
    $recoveryConnections = Get-IcsConnections
    Set-IcsPair ([string]$snapshot.previousPublic) $hotspotGuid $tunnelGuid $recoveryConnections | Out-Null
  } catch {
    # Preserve the original apply error; the caller still needs its cause.
  }
  $snapshot.active = $false
  Save-Snapshot $snapshot
  throw $applyError
}

@{ outcome = 'applied'; transition = $transition } | ConvertTo-Json -Depth 5 -Compress
