BEGIN {
  line = "";
  for (j = 0; j < S; j++) { line = line "x"; }
  for (i = 0; i < N; i++) { print line; }
}

