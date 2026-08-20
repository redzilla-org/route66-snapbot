# Repeats the whole variant matrix N times and prints, per variant/fixture, the
# median-of-medians and the spread across repeats. Each bench.exe invocation is
# itself 15 timed iterations after 3 warmups; repeating the invocation is what
# exposes run-to-run machine noise, which on this host is large enough to swamp
# small transform differences if only one run is taken.
param([int]$Reps = 3)

$variants = @("A", "B", "B2", "D", "D1", "D2", "D3", "D4", "E")
$fixtures = @{ "typical" = "fixtures/typical.png"; "big" = "fixtures/big.png" }
$acc = @{}

for ($r = 0; $r -lt $Reps; $r++) {
  foreach ($v in $variants) {
    foreach ($fx in $fixtures.Keys) {
      $ext = if ($v -eq "A") { "png" } else { "pgm" }
      $out = "out/${fx}_$v.$ext"
      $j = & ./go/bin/bench.exe $v $fixtures[$fx] $out | ConvertFrom-Json
      foreach ($ph in @("decode", "transform", "encode", "total")) {
        $k = "$fx|$v|$ph"
        if (-not $acc.ContainsKey($k)) { $acc[$k] = @() }
        $acc[$k] += $j.$ph.med
      }
    }
  }
  Write-Host "rep $($r+1)/$Reps done"
}

foreach ($fx in @("typical", "big")) {
  Write-Host ""
  Write-Host "== $fx =="
  Write-Host ("{0,-4} {1,-10} {2,-10} {3,-12} {4,-10}" -f "var", "decode", "transform", "encode+wr", "total")
  foreach ($v in $variants) {
    $row = @($v)
    foreach ($ph in @("decode", "transform", "encode", "total")) {
      $s = $acc["$fx|$v|$ph"] | Sort-Object
      $med = $s[[int]($s.Count / 2)]
      $row += ("{0:N2} [{1:N1}-{2:N1}]" -f $med, $s[0], $s[-1])
    }
    Write-Host ("{0,-4} {1,-18} {2,-18} {3,-18} {4,-18}" -f $row[0], $row[1], $row[2], $row[3], $row[4])
  }
}
