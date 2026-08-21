project(
  'oxide-desktop',
  'c',
  version: '0.20.1',
  license: 'GPL-2.0-only',
  meson_version: '>=0.59.0',
  default_options: [
    'c_std=c11',
    'warning_level=2',
  ],
)

add_project_arguments(
  [
    '-DWLR_USE_UNSTABLE',
  ],
  language: 'c',
)

cc = meson.get_compiler('c')

add_project_arguments(cc.get_supported_arguments([
  '-Wundef',
  '-Wlogical-op',
  '-Wmissing-include-dirs',
  '-Wold-style-definition',
  '-Wpointer-arith',
  '-Winit-self',
  '-Wstrict-prototypes',
  '-Wimplicit-fallthrough=2',
  '-Wendif-labels',
  '-Wstrict-aliasing=2',
  '-Woverflow',
  '-Wmissing-prototypes',
  '-Walloca',
  '-Wunused-macros',
  '-Wshadow',

  '-Wno-unused-parameter',
  '-Wno-expansion-to-defined',
]), language: 'c')

version='"@0@"'.format(meson.project_version())
git = find_program('git', native: true, required: false)
if git.found()
  git_commit = run_command([git, 'describe', '--dirty'], check: false)
  if git_commit.returncode() == 0
    version = '"@0@"'.format(git_commit.stdout().strip())
  endif
endif
add_project_arguments('-DOXIDE_DESKTOP_VERSION=@0@'.format(version), language: 'c')

wlroots = dependency(
  'wlroots-0.20',
  default_options: ['default_library=static', 'examples=false'],
  version: ['>=0.20.1', '<0.21.0'],
)

wlroots_has_xwayland = wlroots.get_variable('have_xwayland') == 'true'
have_libsfdo = not get_option('icon').disabled()

wayland_server = dependency('wayland-server', version: '>=1.22.90')
wayland_protos = dependency('wayland-protocols', version: '>=1.39')
xkbcommon = dependency('xkbcommon')
xcb = dependency('xcb', required: get_option('xwayland'))
xcb_ewmh = dependency('xcb-ewmh', required: get_option('xwayland'))
xcb_icccm = dependency('xcb-icccm', required: get_option('xwayland'))
drm_full = dependency('libdrm', required: wlroots.get_variable('have_drm_backend') == 'true')
drm = drm_full.partial_dependency(compile_args: true, includes: true)
xml2 = dependency('libxml-2.0')
glib = dependency('glib-2.0')
cairo = dependency('cairo')
pangocairo = dependency('pangocairo')
input = dependency('libinput', version: '>=1.26', required: wlroots.get_variable('have_libinput_backend') == 'true')
pixman = dependency('pixman-1')
math = cc.find_library('m')
png = dependency('libpng')
svg = dependency('librsvg-2.0', version: '>=2.46', required: false)
sfdo_basedir = dependency(
  'libsfdo-basedir',
  default_options: ['default_library=static', 'examples=false', 'tests=false'],
  version: '>=0.1.3',
  required: have_libsfdo,
)
sfdo_desktop = dependency(
  'libsfdo-desktop',
  default_options: ['default_library=static', 'examples=false', 'tests=false'],
  version: '>=0.1.3',
  required: have_libsfdo,
)
sfdo_icon = dependency(
  'libsfdo-icon',
  default_options: ['default_library=static', 'examples=false', 'tests=false'],
  version: '>=0.1.3',
  required: have_libsfdo,
)

xwayland = dependency(
  'xwayland',
  version: '>=21.1.9',
  required: get_option('xwayland'),
)

if get_option('xwayland').enabled() and not wlroots_has_xwayland
	error('no wlroots Xwayland support')
endif
if get_option('xwayland').disabled()
	have_xwayland = false
elif not xwayland.found()
	warning('disabling xwayland, requires version >= 21.1.9')
	have_xwayland = false
elif xcb.found() and wlroots_has_xwayland
	have_xwayland = true
else
	have_xwayland = false
endif
conf_data = configuration_data()
conf_data.set10('HAVE_XWAYLAND', have_xwayland)

if get_option('svg').disabled()
  have_rsvg = false
else
  have_rsvg = svg.found()
endif
conf_data.set10('HAVE_RSVG', have_rsvg)

conf_data.set10('HAVE_LIBSFDO', have_libsfdo)

foreach sym : ['LIBINPUT_CONFIG_DRAG_LOCK_ENABLED_STICKY', 'LIBINPUT_CONFIG_3FG_DRAG_ENABLED_3FG']
  has_sym = input.type_name() != 'internal' \
              and cc.has_header_symbol('libinput.h', sym, dependencies: input)
	conf_data.set10('HAVE_' + sym, has_sym)
endforeach

if get_option('static_analyzer').enabled()
  add_project_arguments(['-fanalyzer'], language: 'c')
endif

link_args = []
if get_option('sections').enabled()
  add_project_arguments(['-ffunction-sections'], language: 'c')
  link_args += [
    '-Wl,--gc-sections',
    '-Wl,--print-gc-sections',
  ]
endif

msgfmt = find_program('msgfmt', required: get_option('nls'))
if msgfmt.found()
  source_root = meson.current_source_dir()
  conf_data.set('HAVE_NLS', 1)
  subdir('po')
else
  conf_data.set('HAVE_NLS', 0)
endif

oxide_desktop_inc = include_directories('include')

subdir('protocols')

oxide_desktop_deps = [
  server_protos,
  wayland_server,
  wlroots,
  xkbcommon,
	xcb_ewmh,
  xcb_icccm,
  xml2,
  glib,
  cairo,
  drm,
  pangocairo,
  input,
  pixman,
  math,
  png,
]
if have_rsvg
  oxide_desktop_deps += [
    svg,
  ]
endif
if have_libsfdo
  oxide_desktop_deps += [
    sfdo_basedir,
    sfdo_desktop,
    sfdo_icon,
  ]
endif

# ---- panel (out-of-process GTK4 client) -----------------------------------
# The panel is a separate process (clients/oxide-panel) launched by the
# compositor. It is NOT linked into the compositor, so the compositor has no
# GTK dependency and can never deadlock on an in-process client.

subdir('include')
subdir('src')
subdir('docs')
subdir('clients')

dep_cmocka = dependency('cmocka', required: get_option('test'))
if dep_cmocka.found()
  subdir('t')
endif

executable(
  meson.project_name(),
  oxide_desktop_sources,
  include_directories: [oxide_desktop_inc],
  dependencies: oxide_desktop_deps,
  install: true,
  link_args: link_args,
)

install_data('data/oxide-desktop.desktop', install_dir: get_option('datadir') / 'wayland-sessions')
install_data(
  'data/theme/oxide/oxide-desktop/close.xbm',
  'data/theme/oxide/oxide-desktop/max.xbm',
  'data/theme/oxide/oxide-desktop/max_toggled.xbm',
  'data/theme/oxide/oxide-desktop/iconify.xbm',
  install_dir: get_option('datadir') / 'themes/oxide/oxide-desktop',
)

install_data('data/oxide-desktop-portals.conf', install_dir: get_option('datadir') / 'xdg-desktop-portal')

# Install oxide-desktop-session.target so that systemd user services with
# WantedBy=graphical-session.target can be started and stopped in sync
# with an oxide-desktop session (see oxide-desktop(1) SESSION MANAGEMENT for the opt-in
# autostart/shutdown snippet).
systemd_feat = get_option('systemd-session')
systemd = dependency('systemd', required: systemd_feat)
if systemd.found()
  install_data('data/oxide-desktop-session.target',
    install_dir: systemd.get_variable('systemduserunitdir'))
endif

icons = ['oxide-desktop-symbolic.svg', 'oxide-desktop.svg']
foreach icon : icons
  icon_path = join_paths('data', icon)
  install_data(icon_path, install_dir: get_option('datadir') / 'icons/hicolor/scalable/apps')
endforeach
