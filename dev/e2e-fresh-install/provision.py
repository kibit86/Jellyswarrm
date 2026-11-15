import sys
from lib import *
# A fresh Jellyfin: wizard + users created from scratch. name -> (admin account, [(user, password, is_admin)])
SERVERS = {
  "http://127.0.0.1:18096": ("root-a", "rootpw-a", [("owner", "owner-pw-A", True), ("viewer", "viewer-pw-A", False)]),
  "http://127.0.0.1:18097": ("root-b", "rootpw-b", [("boss", "boss-pw-B", True), ("guest", "guest-pw-B", False)]),
}
for base, (root, rootpw, users) in SERVERS.items():
    st, info = call(base, "GET", "/System/Info/Public")
    assert not info["StartupWizardCompleted"], f"{base} is not a fresh install"
    call(base, "POST", "/Startup/Configuration", {"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"})
    call(base, "GET", "/Startup/User"); call(base, "POST", "/Startup/User", {"Name": root, "Password": rootpw})
    call(base, "POST", "/Startup/RemoteAccess", {"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False})
    print(base, "wizard", call(base, "POST", "/Startup/Complete")[0])
    tok = call(base, "POST", "/Users/AuthenticateByName", {"Username": root, "Pw": rootpw})[1]["AccessToken"]
    print("  library", call(base, "POST", "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=true",
                            {"LibraryOptions": {"PathInfos": [{"Path": "/media"}]}}, token=tok)[0])
    for name, pw, admin in users:
        st, u = call(base, "POST", "/Users/New", {"Name": name, "Password": pw}, token=tok)
        if admin:
            full = call(base, "GET", f"/Users/{u['Id']}", token=tok)[1]; full["Policy"]["IsAdministrator"] = True
            call(base, "POST", f"/Users/{u['Id']}/Policy", full["Policy"], token=tok)
        print(f"  user {name} (admin={admin})", st)
