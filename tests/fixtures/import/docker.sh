#!/bin/sh
# A subprocess fixture for import.rs. It never contacts any engine or reads
# outside its per-test directory; the production CLI factory is unchanged.
set -eu
fixture=$ASAGIRI_IMPORT_FIXTURE
scenario=$(cat "$fixture/case")
printf '%s\n' "$*" >> "$fixture/trace"
[ "$1" = --context ] || exit 90
context=$2
shift 2
case "$1 $2" in
  "volume inspect")
    [ "$context" = orbstack ] || exit 91
    [ "$*" = 'volume inspect --format {{json .}} -- fixture-volume' ] || exit 92
    if [ "$scenario" = inspect-error ] || { [ "$scenario" = late-inspect-error ] && [ -f "$fixture/copied" ]; }; then
      echo "source volume unavailable" >&2
      exit 42
    fi
    if [ "$scenario" = wrong-volume ]; then
      echo '{"Name":"another-volume"}'
    elif [ "$scenario" = labels ]; then
      echo '{"Name":"fixture-volume","Labels":{"com.supabase.cli.project":"demo","example":"space and = equals"}}'
    else
      echo '{"Name":"fixture-volume"}'
    fi
    ;;
  "ps --all")
    if [ "$context" = "$ASAGIRI_IMPORT_DESTINATION" ]; then
      [ "$*" = 'ps --all --filter volume=fixture-volume --format {{.Names}}' ] || exit 92
      case "$scenario" in
        destination-attached) echo stopped-database ;;
        destination-list-error) echo 'cannot list destination' >&2; exit 42 ;;
      esac
      exit 0
    fi
    [ "$context" = orbstack ] || exit 91
    [ "$*" = 'ps --all --filter volume=fixture-volume --filter status=running --filter status=paused --filter status=restarting --format {{json .}}' ] || exit 92
    case "$scenario" in
      list-error) echo "source list unavailable" >&2; exit 42 ;;
      invalid-json) echo 'not JSON' ;;
      unknown-state) echo '{"Names":"fixture-writer","State":"unknown"}' ;;
      running|paused|restarting)
        printf '{"Names":"fixture-writer","State":"%s"}\n' "$scenario"
        ;;
      late-writer)
        if [ -f "$fixture/copied" ]; then echo '{"Names":"fixture-writer","State":"running"}'; fi
        ;;
    esac
    ;;
  "image inspect")
    case "$scenario" in helper-*) exit 1 ;; esac
    ;;
  "pull alpine@sha256:"*)
    if [ "$scenario" = helper-pull-error ] && [ "$context" = "$ASAGIRI_IMPORT_DESTINATION" ]; then
      echo "registry unavailable" >&2
      exit 42
    fi
    ;;
  "volume create")
    [ "$context" = "$ASAGIRI_IMPORT_DESTINATION" ] || exit 91
    if [ "$scenario" = labels ]; then
      [ "$#" = 8 ] && [ "$3" = --label ] && [ "$4" = 'com.supabase.cli.project=demo' ] &&
        [ "$5" = --label ] && [ "$6" = 'example=space and = equals' ] &&
        [ "$7" = -- ] && [ "$8" = fixture-volume ] || exit 93
    fi
    if [ "$scenario" = create-error ]; then echo "destination volume unavailable" >&2; exit 42; fi
    ;;
  "run --rm")
    if [ "$context" = orbstack ]; then
      if [ "$scenario" = source-error ]; then echo "source tar failed" >&2; exit 42; fi
      if [ "$scenario" = verbose-source ]; then dd if=/dev/zero bs=65536 count=8 1>&2 2>/dev/null; fi
      printf '\000volume fixture\377\n'
    else
      [ "$context" = "$ASAGIRI_IMPORT_DESTINATION" ] || exit 91
      if [ "$scenario" = write-error ]; then
        dd bs=1 count=4 of="$fixture/copied" 2>/dev/null
        echo "destination tar failed" >&2
        exit 42
      fi
      cat > "$fixture/copied"
    fi
    ;;
  *) echo "unexpected fixture command" >&2; exit 93 ;;
esac
