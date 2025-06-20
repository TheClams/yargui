# YarGui: Yet Another RIF Graphical User Interface

YarGui is a graphical user interface to display complete register structure defined in RIFs file.

It is based on the yarig library (https://github.com/TheClams/).

## Installation
If you have the rust toolchain installed, just run `cargo install yargui`

## Usage
From the command line simply run `yargui` or `yargui path/to/my.rif` to open directly a file.

At the bottom the `Load` button allows to load a new RIF file.

A search box is also available to quickly find a register/field. 
You can provide detailled path like `rif_name.reg_name.field_name`.
The arrows on the left of the search box allows to quickly navigate into multiple matching register/field.

Element of the path of the currently register displayed in the content area is clickable to navigate to its parent.

Clicking on a field display more details on the field (like enum information, detailled description, ...)

## Key-Binding
A few key-binding are available:
 - `ctrl+o` opens the file dialog to load a RIF file
 - `F5` reload the last opened file
 - `ctrl+f` focus the search box
 - `F3` and `shift+F3` allows to go to next and previous match