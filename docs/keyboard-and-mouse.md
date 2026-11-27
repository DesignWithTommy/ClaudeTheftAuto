# Keyboard and mouse

The title reads a gamepad and a keyboard. It does not ask for a mouse. With no controller connected, M1LK gives it a stand-in gamepad driven by the keyboard and mouse. The title shows its own gamepad prompts for that pad. A connected XInput or DualSense controller replaces the stand-in, and the mouse is then left alone.

## Default layout

| Input | Stand-in pad |
| --- | --- |
| W A S D | Left stick |
| Mouse movement, I J K L | Right stick |
| Right mouse button, Z | Left trigger |
| Left mouse button, C | Right trigger |
| Enter | A |
| Backspace | B |
| X | X |
| Y | Y |
| Q / E | Left / right bumper |
| F / H | Left / right stick click |
| Arrow keys | D-pad |
| Esc | Menu |
| Tab | View |

The cursor starts free. Left-click in the game window to start mouse look, which keeps the cursor in the middle of the window. Press F8 to release the cursor; switching to another window releases it too.

## Settings

Settings use the `M1LK_` prefix; the older `VICTORY_` names still work.

- `M1LK_MOUSE_LOOK=0` turns mouse look off and leaves the cursor alone. The mouse buttons still work.
- `M1LK_MOUSE_SENSITIVITY=<number>` scales mouse look. The default is 1, and 2 turns twice as fast.
- `M1LK_KEYBINDS` changes what keys and mouse buttons press on the stand-in pad. It is a comma-separated list of `Key=Target` entries. Each entry replaces what that key did. Names are case-insensitive. An entry that is not understood is left out, with a warning in the log.

```text
M1LK_KEYBINDS=Space=X,LShift=A,F=Y,Mouse4=LB,Enter=None
```

Keys are letters, digits, `F1` to `F12`, `Space`, `Enter`, `Esc`, `Tab`, `Backspace`, `Up`, `Down`, `Left`, `Right`, `LShift`, `RShift`, `LCtrl`, `RCtrl`, `LAlt`, `RAlt`, and `Mouse1` to `Mouse5` (`LMB`, `RMB` and `MMB` also work).

Targets are `A`, `B`, `X`, `Y`, `LB`, `RB`, `LS`, `RS`, `Menu`, `View`, `DUp`, `DDown`, `DLeft`, `DRight`, `LT`, `RT`, and `None` to unbind.

The stick keys (W A S D and I J K L) always move their sticks. Binding one of them also presses its new target.
