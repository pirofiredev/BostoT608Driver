Polished driver for Bosto T608 on KDE.

If you have strange behavior, like lagging or touching doesn't work, it's worth to try this one patch:

https://github.com/DIGImend/10moons-tools

to start that thing on fedora:

1. Install deps (different on other distros)
`sudo dnf install -y git cargo rust rust-std-static libusb1 libusb1-devel systemd-udev`

2. Clone repo
`git clone https://github.com/pirofiredev/BostoT608Driver.git`

3. Go into it
`cd ~/BostoT608Driver/`

5. Build
`cargo build --release`

6. Run the driver
`sudo target/release/mx002`

---

- The orientation is adjustable
  - **After orientation was changed, step 5 is required**
  - 309 line in raw_pen_abs_to_pen_abs_events()
  
  ```txt
     // Opposite/inverted rotation: 180°
            let x_axis = AXIS_MAX - x_axis;
            let y_axis = AXIS_MAX - y_axis;

    // No rotation
            let x_axis = x_axis;
            let y_axis = y_axis;

    // For inverted / 180°:
            let x_axis = AXIS_MAX - x_axis;
            let y_axis = AXIS_MAX - y_axis;

    // For flip horizontal only:
            let x_axis = AXIS_MAX - x_axis;
            let y_axis = y_axis;

    // For flip vertical only
          let x_axis = x_axis;
          let y_axis = AXIS_MAX - y_axis;

    // For 90° counter-clockwise:
          let old_x = x_axis;
          let old_y = y_axis;
          
          let x_axis = AXIS_MAX - old_y;
          let y_axis = old_x;

    // For 90° clockwise:
        let old_x = x_axis;
        let old_y = y_axis;
        
        let x_axis = old_y;
        let y_axis = AXIS_MAX - old_x;
  ```
