import sys
from lib import *
# Server A: set up the ordinary way, from scratch. Server B is left COMPLETELY NEW on purpose:
# its first setup is done in the browser, through the proxy.
base, root, rootpw = "http://127.0.0.1:18096", "root-a", "rootpw-a"
assert not call(base, "GET", "/System/Info/Public")[1]["StartupWizardCompleted"]
call(base, "POST", "/Startup/Configuration", {"UICulture": "en-US", "MetadataCountryCode": "US", "PreferredMetadataLanguage": "en"})
call(base, "GET", "/Startup/User"); call(base, "POST", "/Startup/User", {"Name": root, "Password": rootpw})
call(base, "POST", "/Startup/RemoteAccess", {"EnableRemoteAccess": True, "EnableAutomaticPortMapping": False})
print("A wizard", call(base, "POST", "/Startup/Complete")[0])
tok = call(base, "POST", "/Users/AuthenticateByName", {"Username": root, "Pw": rootpw})[1]["AccessToken"]
print("A library", call(base, "POST", "/Library/VirtualFolders?name=Movies&collectionType=movies&refreshLibrary=true", {"LibraryOptions": {"PathInfos": [{"Path": "/media"}]}}, token=tok)[0])
st, u = call(base, "POST", "/Users/New", {"Name": "owner", "Password": "owner-pw-A"}, token=tok)
full = call(base, "GET", f"/Users/{u['Id']}", token=tok)[1]; full["Policy"]["IsAdministrator"] = True
call(base, "POST", f"/Users/{u['Id']}/Policy", full["Policy"], token=tok)
print("A owner (admin)", st)
b = call("http://127.0.0.1:18097", "GET", "/System/Info/Public")[1]
print("B wizard completed?", b["StartupWizardCompleted"], "(must be False: brand new)")

# A's library scan can finish before the file is seen: ask again until the movie shows up.
import time
for _ in range(30):
    if len(call(base, "GET", "/Items?Recursive=true&IncludeItemTypes=Movie", token=tok)[1]["Items"]) >= 1:
        break
    call(base, "POST", "/Library/Refresh", token=tok); time.sleep(3)
print("A movies:", len(call(base, "GET", "/Items?Recursive=true&IncludeItemTypes=Movie", token=tok)[1]["Items"]))
