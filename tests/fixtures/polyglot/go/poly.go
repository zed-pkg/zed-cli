// Package poly is the polyglot publish fixture (go slice).
package poly

import "fmt"

// Greet greets, the Go way. Mirrors the other four slices exactly.
func Greet(name string) string {
	return fmt.Sprintf("hello, %s", name)
}
