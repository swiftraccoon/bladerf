-- Frozen control-register contract for RF-observatory profile 2. Profile 1
-- keeps its existing package and wire bytes unchanged.

library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

package rf_profile2_p is
    constant P2_ID : std_logic_vector(31 downto 0) := x"52460201";
    constant P2_CAPABILITIES : std_logic_vector(31 downto 0) := x"00001f04";
    constant P2_COMMIT_MAGIC : std_logic_vector(31 downto 0) := x"434d4954";
    constant P2_UNKNOWN_READ : std_logic_vector(31 downto 0) := x"bad0add0";

    constant P2_ADDR_ID : natural := 0;
    constant P2_ADDR_CAPABILITIES : natural := 1;
    constant P2_ADDR_SHADOW_FIRST : natural := 4;
    constant P2_ADDR_COMMIT : natural := 8;
    constant P2_ADDR_STATUS : natural := 9;
    constant P2_ADDR_APPLIED_EPOCH : natural := 10;
    constant P2_ADDR_OUTCOME_EPOCH : natural := 11;
    constant P2_ADDR_APPLIED_FIRST : natural := 12;

    subtype p2_reason_t is std_logic_vector(3 downto 0);
    constant P2_REASON_NONE : p2_reason_t := x"0";
    constant P2_REASON_BAD_MAGIC : p2_reason_t := x"1";
    constant P2_REASON_INCOMPLETE : p2_reason_t := x"2";
    constant P2_REASON_RX_ACTIVE : p2_reason_t := x"3";
    constant P2_REASON_EPOCH_EXHAUSTED : p2_reason_t := x"4";
    constant P2_REASON_APPLY_REJECTED : p2_reason_t := x"5";

    function p2_epoch_available(epoch : unsigned(31 downto 0)) return boolean;
    function p2_candidate_epoch(epoch : unsigned(31 downto 0)) return unsigned;
    function p2_commit_reason(
        commit_word : std_logic_vector(31 downto 0);
        rx_active : std_logic;
        written_mask : std_logic_vector(3 downto 0);
        applied_epoch : unsigned(31 downto 0)
    ) return p2_reason_t;
    function p2_status(
        busy : std_logic;
        outcome_valid : std_logic;
        outcome_success : std_logic;
        rx_active : std_logic;
        reason : p2_reason_t;
        written_mask : std_logic_vector(3 downto 0)
    ) return std_logic_vector;
end package;

package body rf_profile2_p is
    function p2_epoch_available(epoch : unsigned(31 downto 0)) return boolean is
    begin
        return epoch /= unsigned'(x"ffffffff");
    end function;

    function p2_candidate_epoch(epoch : unsigned(31 downto 0)) return unsigned is
    begin
        if p2_epoch_available(epoch) then
            return epoch + 1;
        end if;
        return epoch;
    end function;

    function p2_commit_reason(
        commit_word : std_logic_vector(31 downto 0);
        rx_active : std_logic;
        written_mask : std_logic_vector(3 downto 0);
        applied_epoch : unsigned(31 downto 0)
    ) return p2_reason_t is
    begin
        if commit_word /= P2_COMMIT_MAGIC then
            return P2_REASON_BAD_MAGIC;
        elsif rx_active /= '0' then
            return P2_REASON_RX_ACTIVE;
        elsif written_mask /= "1111" then
            return P2_REASON_INCOMPLETE;
        elsif not p2_epoch_available(applied_epoch) then
            return P2_REASON_EPOCH_EXHAUSTED;
        end if;
        return P2_REASON_NONE;
    end function;

    function p2_status(
        busy : std_logic;
        outcome_valid : std_logic;
        outcome_success : std_logic;
        rx_active : std_logic;
        reason : p2_reason_t;
        written_mask : std_logic_vector(3 downto 0)
    ) return std_logic_vector is
        variable value : std_logic_vector(31 downto 0) := (others => '0');
    begin
        value(31) := busy;
        value(30) := outcome_valid;
        value(29) := outcome_success;
        value(28) := rx_active;
        value(27 downto 24) := reason;
        value(23 downto 20) := written_mask;
        return value;
    end function;
end package body;
