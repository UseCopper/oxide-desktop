scdoc = find_program('scdoc', required: get_option('man-pages'))

if scdoc.found()
  manpages = [
    'oxide-desktop.1',
    'oxide-desktop-actions.5',
    'oxide-desktop-config.5',
    'oxide-desktop-menu.5',
    'oxide-desktop-theme.5',
    'oxide-nag.1',
  ]
  foreach manpage : manpages
    markdown = manpage + '.scd'
    custom_target(
      manpage,
      input: markdown,
      output: manpage,
      command: scdoc,
      feed: true,
      capture: true,
      install: true,
      install_dir: join_paths(get_option('mandir'), 'man' + manpage.split('.')[-1])
    )
  endforeach
endif

install_data(
  [
    'autostart',
    'environment',
    'menu.xml',
    'README',
    'shutdown',
    'themerc',
    'rc.xml',
    'rc.xml.all'
  ],
  install_dir: get_option('datadir') / 'doc' / meson.project_name()
)
