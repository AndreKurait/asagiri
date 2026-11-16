# Hyperfine 1.x exported seconds on the result itself. Schema 2 declares
# seconds in summary.time_wall_clock, independently of its primary metric.
def hyperfine_wall_clock:
  if (.results | type) != "array" or (.results | length) == 0 then
    error("missing Hyperfine results")
  elif ((.schema_version // 1) == 1) and (.results[0] | has("summary") | not) then
    .results[0]
  elif .schema_version == 2 and .results[0].summary.time_wall_clock.unit == "second" then
    .results[0].summary.time_wall_clock
  else
    error("unsupported Hyperfine schema or wall-clock unit")
  end
  | if type == "object"
       and (.median | type) == "number" and (.min | type) == "number"
       and (.median | isfinite) and (.min | isfinite)
       and .median > 0 and .min >= 0 and .min <= .median then .
    else error("invalid Hyperfine wall-clock statistics")
    end;
