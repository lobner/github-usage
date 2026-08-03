//go:build dumpicons

// Diagnostic: writes the rendered icons to PNG files so they can be eyeballed.
// Excluded from normal runs.
//
//	ICON_DUMP_DIR=/tmp go test -tags dumpicons -run TestDump ./internal/icon
package icon

import (
	"os"
	"path/filepath"
	"testing"
)

func TestDump(t *testing.T) {
	dir := os.Getenv("ICON_DUMP_DIR")
	if dir == "" {
		dir = "."
	}
	for name, data := range map[string][]byte{
		"icon-base.png":     BaseTemplatePNG(),
		"icon-alert.png":    AlertPNG(),
		"icon-incident.png": IncidentPNG(),
	} {
		p := filepath.Join(dir, name)
		if err := os.WriteFile(p, data, 0o644); err != nil {
			t.Fatalf("write %s: %v", p, err)
		}
		t.Logf("wrote %s (%d bytes)", p, len(data))
	}
}
