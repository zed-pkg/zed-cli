// Stripped at publish time by the `**/*_test.go` default exclude.
package poly

import "testing"

func TestGreet(t *testing.T) {
	if got := Greet("zed"); got != "hello, zed" {
		t.Fatalf("got %q", got)
	}
}
