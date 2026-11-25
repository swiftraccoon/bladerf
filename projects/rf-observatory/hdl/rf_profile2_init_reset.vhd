-- Image/PLL-lifetime initialization reset for profile-2 persistent control.
-- The hosted FX3 system reset is a stream-lifecycle event and intentionally
-- does not enter this block. Loss of the system PLL asynchronously asserts both
-- outputs. After lock and a running system clock, the system-domain output is
-- held for G_HOLD_CYCLES complete edges and then synchronously released. The RX
-- output asynchronously asserts from that same event and synchronously releases
-- through two RX-clock registers. If either clock is absent during release,
-- that domain's reset remains asserted rather than relying on FPGA power-up
-- state.

library ieee;
use ieee.std_logic_1164.all;

entity rf_profile2_init_reset is
    generic (
        G_HOLD_CYCLES : positive := 16
    );
    port (
        sys_clock : in std_logic;
        sys_pll_locked : in std_logic;
        rx_clock : in std_logic;
        sys_reset : out std_logic;
        rx_reset : out std_logic
    );
end entity;

architecture rtl of rf_profile2_init_reset is
    attribute ALTERA_ATTRIBUTE : string;
    attribute PRESERVE : boolean;

    signal hold_count : natural range 0 to G_HOLD_CYCLES := 0;
    signal sys_reset_i : std_logic := '1';
    signal rx_reset_meta : std_logic := '1';
    signal rx_reset_sync : std_logic := '1';

    attribute ALTERA_ATTRIBUTE of rx_reset_meta : signal is
        "-name SYNCHRONIZER_IDENTIFICATION ""FORCED IF ASYNCHRONOUS""";
    attribute ALTERA_ATTRIBUTE of rx_reset_sync : signal is
        "-name SYNCHRONIZER_IDENTIFICATION ""FORCED IF ASYNCHRONOUS""";
    attribute PRESERVE of rx_reset_meta : signal is true;
    attribute PRESERVE of rx_reset_sync : signal is true;
begin
    sys_reset <= sys_reset_i;
    rx_reset <= rx_reset_sync;

    stretch_system_reset : process(sys_clock, sys_pll_locked)
    begin
        if sys_pll_locked /= '1' then
            hold_count <= 0;
            sys_reset_i <= '1';
        elsif rising_edge(sys_clock) then
            if hold_count < G_HOLD_CYCLES then
                hold_count <= hold_count + 1;
                sys_reset_i <= '1';
            else
                sys_reset_i <= '0';
            end if;
        end if;
    end process;

    synchronize_rx_release : process(rx_clock, sys_reset_i)
    begin
        if sys_reset_i = '1' then
            rx_reset_meta <= '1';
            rx_reset_sync <= '1';
        elsif rising_edge(rx_clock) then
            rx_reset_meta <= '0';
            rx_reset_sync <= rx_reset_meta;
        end if;
    end process;
end architecture;
