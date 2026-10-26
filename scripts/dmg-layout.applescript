-- Keep the 800 × 540 canvas and Applications position in sync with dmg-background.swift.
on run arguments
    set volumeFolder to POSIX file (item 1 of arguments) as alias
    set appName to item 2 of arguments
    set backgroundFile to POSIX file ((item 1 of arguments) & "/.background/install.tiff") as alias

    tell application "Finder"
        open volumeFolder
        set installerWindow to container window of volumeFolder
        set current view of installerWindow to icon view
        set toolbar visible of installerWindow to false
        set statusbar visible of installerWindow to false
        set pathbar visible of installerWindow to false
        -- Finder window bounds include the 28-point title bar.
        set bounds of installerWindow to {160, 120, 960, 688}
        tell icon view options of installerWindow
            set arrangement to not arranged
            set icon size to 144
            set text size to 16
            set label position to bottom
            set shows item info to false
            set shows icon preview to false
            set background picture to backgroundFile
        end tell
        set position of item appName of volumeFolder to {230, 270}
        set position of item "Applications" of volumeFolder to {550, 270}
        update volumeFolder without registering applications
        delay 1
        close installerWindow
    end tell
end run
