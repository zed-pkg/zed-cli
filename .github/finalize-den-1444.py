from pathlib import Path

runtime = Path("src/task_runtime.rs")
text = runtime.read_text()

old_tuple = "        let (states, ready) = &*self.states;\n"
new_tuple = "        let (state_mutex, ready) = &*self.states;\n"
if text.count(old_tuple) != 1:
    raise SystemExit(
        f"expected one task state tuple anchor, found {text.count(old_tuple)}"
    )
text = text.replace(old_tuple, new_tuple, 1)

old_lock = "        let mut states = states.lock().expect(\"task state mutex poisoned\");\n"
new_lock = (
    "        let mut states = state_mutex\n"
    "            .lock()\n"
    "            .expect(\"task state mutex poisoned\");\n"
)
if text.count(old_lock) != 2:
    raise SystemExit(
        f"expected two task state lock anchors, found {text.count(old_lock)}"
    )
text = text.replace(old_lock, new_lock)

old_fixture = '            depends-post = ["cleanup"]\n'
new_fixture = '            depends_post = ["cleanup"]\n'
if text.count(old_fixture) != 1:
    raise SystemExit(
        f"expected one depends-post fixture anchor, found {text.count(old_fixture)}"
    )
text = text.replace(old_fixture, new_fixture, 1)

runtime.write_text(text)
