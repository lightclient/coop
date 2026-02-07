def ansi(c): "\u001b[\(c)m";
def reset: ansi("0");
def bold: ansi("1");
def dim: ansi("2");

def level_color:
  if .level == "ERROR" then ansi("1;31")
  elif .level == "WARN"  then ansi("1;33")
  elif .level == "INFO"  then ansi("1;32")
  elif .level == "DEBUG" then ansi("36")
  else ansi("90") end;

def level_tag:
  if .level == "ERROR" then "ERR"
  elif .level == "WARN"  then "WRN"
  elif .level == "INFO"  then "INF"
  elif .level == "DEBUG" then "DBG"
  elif .level == "TRACE" then "TRC"
  else .level[0:3] end;

def fmt_kv:
  [ to_entries[]
    | select(.key != "message"
         and .key != "name"
         and .key != "log.target"
         and .key != "log.module_path"
         and .key != "log.file"
         and .key != "log.line")
    | "\(ansi("1;37"))\(.key)\(reset)=\(ansi("33"))\(.value)\(reset)"
  ] | join(" ");

def fmt_fields:
  [ (.fields | fmt_kv), (.span // {} | fmt_kv) ]
  | map(select(length > 0)) | join(" ");

(.timestamp | split("T")[1] | split(".")[0]) as $time |
level_color as $lc |

(if .span.name then "\(ansi("35"))\(.span.name)\(reset) " else "" end) as $span |

(if (.spans | length) > 0
 then "\(dim)\([.spans[].name] | join(" → "))\(reset) › "
 else "" end) as $ancestry |

fmt_fields as $flds |

"\(dim)\($time)\(reset) \($lc)\(level_tag)\(reset) \($ancestry)\($span)\(bold)\(.fields.message // "")\(reset)\(if ($flds | length) > 0 then " \($flds)" else "" end)"
