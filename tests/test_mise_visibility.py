import pathlib,re,unittest
ROOT=pathlib.Path(__file__).resolve().parents[1]
class MiseVisibilityTests(unittest.TestCase):
 def test_only_wrapper_is_public_and_raw_module_is_private(self):
  lib=(ROOT/"src/lib.rs").read_text()
  self.assertRegex(lib,r'(?m)^mod mise_export_impl;$')
  self.assertRegex(lib,r'(?m)^pub mod mise_export \{$')
  self.assertNotRegex(lib,r'(?m)^pub mod mise_export_impl;')
  for symbol in ("MiseExportAction","MiseExportMode","MiseExportReport","print_export"):
   self.assertIn(symbol,lib)
 def test_no_unreachable_pub_suppression_added(self):
  lib=(ROOT/"src/lib.rs").read_text()
  self.assertNotIn("allow(unreachable_pub)",lib)
  self.assertNotIn("allow(warnings)",lib)
 def test_project_lock_public_contract_does_not_disappear(self):
  s=(ROOT/"src/project_lock.rs").read_text()
  for symbol in ["pub struct OperationGuard","pub fn operation_lock_path","pub fn acquire","pub fn with_lock"]:
   self.assertIn(symbol,s)
if __name__=="__main__":unittest.main()
