$d = Join-Path ([IO.Path]::GetTempPath()) 'rig-@NONCE@'
foreach ($w in '@ASK@'.Split(' ')) {
  $b = [IO.File]::ReadAllText("$d\" + $w.Substring(0, 1)); $i = [int]$w.Substring(1)
  [Console]::Out.WriteLine('~' + $w + '~' + $b.Substring($i * @LINE@, [Math]::Min(@LINE@, $b.Length - $i * @LINE@)))
}
[Console]::Out.WriteLine('#RIG@NONCE@.end#')
