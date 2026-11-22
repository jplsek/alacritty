# Source this in an interactive zsh to report command completion to Alacritty.
[[ -o interactive ]] || return 0

autoload -Uz add-zsh-hook
typeset -gi _alacritty_command_running=0

_alacritty_command_started() {
    # Report the typed command with OSC 633 escaping, followed by its start mark.
    local command_line=${1[1,512]}
    command_line=${command_line//[[:cntrl:]]/ }
    command_line=${command_line//\\/\\\\}
    command_line=${command_line//;/\\x3b}
    builtin printf '\033]633;E;%s\007\033]133;C\007' "$command_line"
    _alacritty_command_running=1
}

_alacritty_command_finished() {
    local exit_status=$?
    if (( _alacritty_command_running )); then
        _alacritty_command_running=0
        builtin printf '\033]133;D;%d\007' "$exit_status"
    fi
}

add-zsh-hook preexec _alacritty_command_started
add-zsh-hook precmd _alacritty_command_finished
