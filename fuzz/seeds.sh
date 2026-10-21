#!/usr/bin/env bash
# Regenerates the gitignored in/ and edge.dict from tests/cases/vm and dict.txt.
set -euo pipefail
cd "$(dirname "$0")"

# One seed file per case of tests/cases/vm, a block off its indent or a quoted one unescaped, named by its hash.
rm -rf in && mkdir -p in
perl -MDigest::SHA=sha1_hex -e '
    sub seed { my $s = shift; return if $s eq ""; open my $out, ">", "in/" . substr(sha1_hex($s), 0, 16) or die $!; print $out $s; }
    my $block;
    while (my $l = <>) {
        if (defined $block) {
            if ($l =~ /^(    |\n)/) { $l =~ s/^    //; $block .= $l; next; }
            $block =~ s/\n+\z//; seed($block); undef $block;
        }
        if ($l =~ /^- case: \|/) { $block = ""; }
        elsif ($l =~ /^- case: "((?:[^"\\]|\\.)*)"/) {
            my $s = $1;
            $s =~ s/\\(.)/$1 eq "n" ? "\n" : $1 eq "t" ? "\t" : $1 eq "r" ? "\r" : $1/ge;
            seed($s);
        }
    }
    if (defined $block) { $block =~ s/\n+\z//; seed($block); }' ../tests/cases/vm/*.yml
echo "seeds: $(ls in | wc -l | tr -d ' ')"

cp dict.txt edge.dict
echo "dict: $(grep -c '^"' edge.dict) entries"
