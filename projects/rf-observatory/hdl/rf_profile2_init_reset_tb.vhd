-- Clock-loss, minimum hold and cross-domain release contract for the profile-2
-- image-lifetime reset.

library ieee;
use ieee.std_logic_1164.all;
use std.env.all;

entity rf_profile2_init_reset_tb is end entity;

architecture simulation of rf_profile2_init_reset_tb is
    constant HOLD_CYCLES : positive := 16;
    signal sys_clock : std_logic := '0';
    signal rx_clock : std_logic := '0';
    signal sys_pll_locked : std_logic := '0';
    signal sys_reset, rx_reset : std_logic;
begin
    sys_clock <= not sys_clock after 5 ns;
    rx_clock <= not rx_clock after 7 ns;

    dut : entity work.rf_profile2_init_reset
        generic map (G_HOLD_CYCLES => HOLD_CYCLES)
        port map (
            sys_clock => sys_clock,
            sys_pll_locked => sys_pll_locked,
            rx_clock => rx_clock,
            sys_reset => sys_reset,
            rx_reset => rx_reset
        );

    stimulus : process
        variable rx_edges : natural;
    begin
        wait for 2 ns;
        assert sys_reset = '1' and rx_reset = '1'
            report "unlocked startup did not assert both init resets"
            severity failure;

        sys_pll_locked <= 'X';
        wait for 1 ns;
        assert sys_reset = '1' and rx_reset = '1'
            report "unknown PLL-lock state did not fail closed"
            severity failure;
        sys_pll_locked <= '0';

        wait until falling_edge(sys_clock);
        sys_pll_locked <= '1';
        for edge in 1 to HOLD_CYCLES loop
            wait until rising_edge(sys_clock);
            wait for 1 ns;
            assert sys_reset = '1'
                report "system init reset released before its full hold"
                severity failure;
        end loop;
        wait until rising_edge(sys_clock);
        wait for 1 ns;
        assert sys_reset = '0'
            report "system init reset did not release after its full hold"
            severity failure;

        rx_edges := 0;
        while rx_reset = '1' loop
            wait until rising_edge(rx_clock);
            wait for 1 ns;
            rx_edges := rx_edges + 1;
            assert rx_edges <= 2
                report "RX init reset release exceeded two RX edges"
                severity failure;
        end loop;
        assert rx_edges = 2
            report "RX init reset bypassed its two-stage release"
            severity failure;

        -- PLL loss is asynchronous to both clocks and must assert immediately.
        wait until falling_edge(sys_clock);
        wait for 2 ns;
        sys_pll_locked <= '0';
        wait for 1 ns;
        assert sys_reset = '1' and rx_reset = '1'
            report "PLL loss did not asynchronously assert init resets"
            severity failure;

        -- A brief relock cannot release reset; all 16 new system edges are
        -- required again. Losing lock midway restarts the count.
        sys_pll_locked <= '1';
        for edge in 1 to 8 loop
            wait until rising_edge(sys_clock);
        end loop;
        assert sys_reset = '1'
            report "partial relock hold released system reset" severity failure;
        sys_pll_locked <= '0';
        wait for 1 ns;
        sys_pll_locked <= '1';
        for edge in 1 to HOLD_CYCLES loop
            wait until rising_edge(sys_clock);
            wait for 1 ns;
            assert sys_reset = '1'
                report "restarted hold released system reset early"
                severity failure;
        end loop;
        wait until rising_edge(sys_clock);
        wait for 1 ns;
        assert sys_reset = '0'
            report "restarted hold did not release system reset"
            severity failure;

        rx_edges := 0;
        while rx_reset = '1' loop
            wait until rising_edge(rx_clock);
            wait for 1 ns;
            rx_edges := rx_edges + 1;
            assert rx_edges <= 2
                report "second RX reset release exceeded two RX edges"
                severity failure;
        end loop;
        assert rx_edges = 2
            report "second RX reset release bypassed its two stages"
            severity failure;

        stop;
        wait;
    end process;
end architecture;
