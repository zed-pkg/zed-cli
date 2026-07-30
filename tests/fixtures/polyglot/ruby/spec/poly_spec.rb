# Stripped at publish time by the `spec/**` default exclude.
require "zed_poly"

raise "greet broken" unless ZedPoly.greet("zed") == "hello, zed"
