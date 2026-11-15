# Calendar widget

The next hour of your Google Calendar, drawn inside the Windows taskbar:

- A red dot marks **now**, and a red line runs one hour to the right.
- The meetings in that hour sit above the line as colored blocks, in their Google Calendar colors.
- Invitations you haven't answered yet show as outlined blocks.

Using it:

- **Hover** to see the rest of today's events with their times.
- **Click** an event to join its Google Meet call, or to open it if it has no Meet link.
- **Drag** the widget left or right to move it. It remembers where you put it.
- **Right-click** it for *Refresh*, *Open Google Calendar*, *Sign in / Sign out*, *Reset position* and *Exit*.

Events refresh every 2 minutes, and right after the PC wakes from sleep. All-day events,
working-location entries and declined invitations are hidden.

## What you need

- Windows 10 or 11, with the taskbar at the bottom of the screen
- A Google account (personal Gmail or Google Workspace)
- About 10 minutes for the one-time Google setup below

## Why there is a Google setup step

The widget talks directly to Google from your PC. It has no server and doesn't send your
calendar anywhere else. To do that, Google needs a "client ID" for it, which you create once
in your own Google Cloud account. It's free, and you won't need to enter a credit card.

The widget only asks for permission to **read** your calendar. Your sign-in is saved in
`%USERPROFILE%\.claude\calendar-widget\token.bin`, encrypted so that only your Windows
account can read it.

## Install

### Step 1: Download the widget

1. Open the [Releases page](https://github.com/domehank/calendar-widget/releases/latest) and download `calendar-widget.exe`.
2. Make a folder for it, for example `C:\Users\<you>\Apps\`, and move the file there.
3. Double-click it. The widget appears in the taskbar, next to the clock and tray icons.
   It also creates the folder `C:\Users\<you>\.claude\calendar-widget\`, which you'll use in step 2.

Windows may show a blue **"Windows protected your PC"** box, because the program isn't
signed with a paid certificate. Click **More info**, then **Run anyway**. You only need to do
this once. If you'd rather not run a downloaded program, you can build it yourself (see
[Build from source](#build-from-source)).

### Step 2: Create your Google client ID (one time)

1. Go to the [Google Cloud Console](https://console.cloud.google.com/) and sign in with the
   Google account whose calendar you want to see.
2. At the top left, click the project picker, then **New project**. Name it, for example,
   `Calendar widget`, and click **Create**. Make sure the new project is selected.
3. Turn on the Calendar API: open
   [APIs & Services → Library](https://console.cloud.google.com/apis/library), search for
   **Google Calendar API**, open it and click **Enable**.
4. Set up the consent screen: open **Google Auth Platform** (in older consoles: **APIs &
   Services → OAuth consent screen**) and click **Get started**.
   - App name: `Calendar widget`. Support email: your email.
   - Audience:
     - **Internal** if you have a Google Workspace (work or school) account. This is the simplest option.
     - **External** if you use a personal Gmail account.
   - Contact email: your email. Accept the policy and click **Create**.
5. **Only if you chose External:** open **Audience**. Under **Test users**, click **Add users**
   and add your own Gmail address. (See [Google sign-in expires](#troubleshooting) below.)
6. Create the client ID: open **Clients** (or **Credentials → Create credentials → OAuth
   client ID**), click **Create client**, choose application type **Desktop app**, name it
   and click **Create**.
7. Click **Download JSON**. You get a file named like
   `client_secret_1234-abcd.apps.googleusercontent.com.json`.
8. Move that file into `C:\Users\<you>\.claude\calendar-widget\`. Don't rename it.

Keep this file to yourself. Don't share it or upload it anywhere.

### Step 3: Sign in

1. Click the widget. Your browser opens the Google sign-in page.
2. Choose your account and allow access to your calendar.
   - With **External**, Google first warns that it **"hasn't verified this app"**. That's
     expected, because the app is yours. Click **Continue** (or **Advanced → Go to Calendar widget**).
3. The browser says you can close the tab, and the widget shows your next hour.

### Step 4: Start it when Windows starts (optional)

1. Press **Win + R**, type `shell:startup` and press **Enter**. A folder opens.
2. Right-click `calendar-widget.exe` in your Apps folder, choose **Show more options →
   Create shortcut**, and move the new shortcut into the Startup folder.

## Uninstall

1. Right-click the widget, choose **Sign out**, then **Exit**.
2. Delete the shortcut from the Startup folder (`shell:startup`) and delete `calendar-widget.exe`.
3. Delete the folder `C:\Users\<you>\.claude\calendar-widget\`.
4. To remove its access from your Google account, go to
   [myaccount.google.com/permissions](https://myaccount.google.com/permissions) and remove *Calendar widget*.
   You can also delete the Google Cloud project.

## Troubleshooting

| Problem | What to do |
| --- | --- |
| Hovering says to put the OAuth client JSON in a folder | Step 2 isn't finished. The file must be in that exact folder, and its name must start with `client_secret` and end with `.json`. |
| Google sign-in expires after a week | Google does this for **External** apps in *Testing* mode. To stop it, open **Google Auth Platform → Audience** and click **Publish app**. You don't need to submit it for review. You'll see the "hasn't verified this app" warning when you sign in again, which is fine. |
| "Access blocked" or error 403 when signing in | With **External**, add your own address as a test user (step 2.5). Also check that the Google Calendar API is enabled (step 2.3). |
| The widget isn't visible | It may be behind other taskbar icons. Exit it in Task Manager and start it again, then right-click it and choose **Reset position**. |
| An event is missing | All-day events, working-location entries and declined invitations are hidden on purpose. Only your main calendar is shown. |

## Build from source

1. Install Rust from [rustup.rs](https://rustup.rs/) and accept the defaults.
2. Download this project (green **Code** button → **Download ZIP**, then unzip it), open a terminal in that folder and run:

   ```
   cargo build --release
   ```

   The first build downloads the Rust toolchain named in `rust-toolchain.toml`, so it takes
   a few minutes. You don't need Visual Studio.
3. The program is at `target\release\calendar-widget.exe`. If an older copy is running, exit
   it first (right-click → Exit).

To try the look without a Google account, set the environment variable
`CALENDAR_WIDGET_DEMO=1` before starting it. The widget then shows made-up events:

```
set CALENDAR_WIDGET_DEMO=1
target\release\calendar-widget.exe
```

Files the widget uses, all in `%USERPROFILE%\.claude\calendar-widget\`:

- `client_secret_….json`: your Google client ID (from step 2)
- `token.bin`: your sign-in, encrypted with Windows DPAPI
- `position.json`: the widget's saved position
